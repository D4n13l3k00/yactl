use serde_json::{Value, json};

#[derive(Clone, Copy, Debug)]
pub(super) enum Service {
    Aliced,
    Alarmd,
}
#[derive(Clone, Copy)]
enum Wire<'a> {
    Number(u64),
    Bytes(&'a [u8]),
    Fixed,
}
fn take<'a>(bytes: &'a [u8], offset: &mut usize, size: usize) -> Result<&'a [u8], ()> {
    let end = offset.checked_add(size).ok_or(())?;
    let result = bytes.get(*offset..end).ok_or(())?;
    *offset = end;
    Ok(result)
}
fn varint(bytes: &[u8], offset: &mut usize) -> Result<u64, ()> {
    let mut value = 0;
    for index in 0..10 {
        let byte = take(bytes, offset, 1)?[0];
        if index == 9 && byte > 1 {
            return Err(());
        }
        value |= u64::from(byte & 127) << (index * 7);
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    Err(())
}
fn fields(bytes: &[u8]) -> Result<Vec<(u32, Wire<'_>)>, ()> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if result.len() == 128 {
            return Err(());
        }
        let key = varint(bytes, &mut offset)?;
        let number = key >> 3;
        if number == 0 || number > 0x1fff_ffff {
            return Err(());
        }
        let value = match key & 7 {
            0 => Wire::Number(varint(bytes, &mut offset)?),
            2 => {
                let size = usize::try_from(varint(bytes, &mut offset)?).map_err(|_| ())?;
                Wire::Bytes(take(bytes, &mut offset, size)?)
            }
            1 | 5 => {
                take(bytes, &mut offset, if key & 7 == 1 { 8 } else { 4 })?;
                Wire::Fixed
            }
            _ => return Err(()),
        };
        result.push((number as u32, value));
    }
    Ok(result)
}
struct Message<'a>(Vec<(u32, Wire<'a>)>);
impl<'a> Message<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, ()> {
        Ok(Self(fields(bytes)?))
    }
    fn one(&self, number: u32) -> Result<Option<Wire<'a>>, ()> {
        let mut matches = self.0.iter().filter(|(field, _)| *field == number);
        let value = matches.next().map(|(_, value)| *value);
        if matches.next().is_some() {
            return Err(());
        }
        Ok(value)
    }
    fn bytes(&self, number: u32) -> Result<Option<&'a [u8]>, ()> {
        match self.one(number)? {
            Some(Wire::Bytes(value)) => Ok(Some(value)),
            None => Ok(None),
            _ => Err(()),
        }
    }
    fn text(&self, number: u32) -> Result<Option<&'a str>, ()> {
        self.bytes(number)?
            .map(|bytes| {
                if bytes.len() > 256 {
                    return Err(());
                }
                std::str::from_utf8(bytes).map_err(|_| ())
            })
            .transpose()
    }
    fn number(&self, number: u32) -> Result<Option<u64>, ()> {
        match self.one(number)? {
            Some(Wire::Number(value)) => Ok(Some(value)),
            None => Ok(None),
            _ => Err(()),
        }
    }
    fn payload(&self, number: u32) -> Result<Option<&'a [u8]>, ()> {
        let body = self.bytes(252)?;
        let kind = self.number(253)?;
        match (body, kind) {
            (None, None) => self.bytes(number),
            (Some(body), Some(kind)) if (1..=0x1fff_ffff).contains(&kind) => {
                // A direct payload alongside a wrapper has ambiguous meaning.
                if self.one(3)?.is_some() || self.one(215)?.is_some() {
                    return Err(());
                }
                Ok((kind == u64::from(number)).then_some(body))
            }
            _ => Err(()),
        }
    }
}
pub(super) fn decode(service: Service, bytes: &[u8]) -> Result<Option<(&'static str, Value)>, ()> {
    if bytes.len() > 65536 {
        return Err(());
    }
    let envelope = Message::parse(bytes)?;
    match service {
        Service::Aliced => {
            let Some(state) = envelope.payload(3)? else {
                return Ok(None);
            };
            let state = Message::parse(state)?;
            if let Some(source) = state.bytes(10)? {
                let source = Message::parse(source)?;
                if source.text(6)? == Some("command")
                    && let Some(request) = state.bytes(15)?
                {
                    let request = Message::parse(request)?;
                    let text = request.text(1)?.filter(|text| !text.is_empty());
                    if request.number(2)?.is_some_and(|flag| flag != 0)
                        && let Some(text) = text
                    {
                        return Ok(Some((
                            "speech",
                            json!({"source":"command","transcript":text}),
                        )));
                    }
                }
            }
            Ok(state
                .text(2)?
                .filter(|text| !text.is_empty())
                .map(|text| ("speech", json!({"source":"voice","transcript":text}))))
        }
        Service::Alarmd => {
            let Some(event) = envelope.payload(215)? else {
                return Ok(None);
            };
            let event = Message::parse(event)?;
            let Some(alarm) = event.bytes(1)? else {
                return Ok(None);
            };
            let alarm = Message::parse(alarm)?;
            Ok(Some((
                "alarm_fired",
                json!({
                    "id":alarm.text(1)?, "alarm_type":alarm.number(2)?,
                    "start_timestamp_ms":alarm.number(3)?, "duration_seconds":alarm.number(4)?,
                    "remind_message":alarm.text(5)?, "currently_playing":alarm.number(6)?.map(|v| v != 0),
                }),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_direct_envelopes_decode_synthetic_speech_and_alarm_fields() {
        let speech = [0x1a, 4, 0x12, 2, b'h', b'i'];
        assert_eq!(
            decode(Service::Aliced, &speech),
            Ok(Some((
                "speech",
                json!({"source":"voice","transcript":"hi"})
            )))
        );
        let alarm = [
            0xba, 0x0d, 19, 0x0a, 17, 0x0a, 1, b'a', 0x10, 3, 0x18, 0x96, 1, 0x20, 60, 0x2a, 3,
            b't', b'e', b'a', 0x30, 1,
        ];
        assert_eq!(
            decode(Service::Alarmd, &alarm),
            Ok(Some((
                "alarm_fired",
                json!({"id":"a","alarm_type":3,"start_timestamp_ms":150,"duration_seconds":60,"remind_message":"tea","currently_playing":true})
            )))
        );
        assert_eq!(decode(Service::Aliced, &[0x08, 1]), Ok(None));
        // Fixed-width unknown fields are skipped, not decoded as text.
        assert_eq!(
            decode(
                Service::Aliced,
                &[0x09, 0, 0, 0, 0, 0, 0, 0, 0, 0x15, 0, 0, 0, 0]
            ),
            Ok(None)
        );
        assert_eq!(decode(Service::Aliced, &[0x1a, 0]), Ok(None));
        assert_eq!(decode(Service::Aliced, &[0x1a, 2, 0x12, 0]), Ok(None));
    }

    #[test]
    fn wrapped_speech_and_alarm_use_the_same_validated_payloads_as_direct_frames() {
        let speech = [0xe2, 0x0f, 4, 0x12, 2, b'h', b'i', 0xe8, 0x0f, 3];
        assert_eq!(
            decode(Service::Aliced, &speech),
            Ok(Some((
                "speech",
                json!({"source":"voice","transcript":"hi"})
            )))
        );
        assert_eq!(decode(Service::Alarmd, &speech), Ok(None));
        let alarm = [0xe2, 0x0f, 5, 0x0a, 3, 0x0a, 1, b'a', 0xe8, 0x0f, 0xd7, 1];
        let decoded = decode(Service::Alarmd, &alarm).unwrap().unwrap();
        assert_eq!(decoded.0, "alarm_fired");
        assert_eq!(decoded.1["id"], "a");
        assert_eq!(
            decode(Service::Aliced, &[0xe2, 0x0f, 1, 0, 0xe8, 0x0f, 0xd2, 1]),
            Ok(None)
        );
    }

    #[test]
    fn only_final_commands_are_emitted_and_take_priority_over_voice_text() {
        let mut frame = vec![
            0xe2, 0x0f, 25, 0x52, 9, 0x32, 7, b'c', b'o', b'm', b'm', b'a', b'n', b'd', 0x7a, 8,
            0x0a, 4, b's', b't', b'o', b'p', 0x10, 1, 0x12, 2, b'h', b'i', 0xe8, 0x0f, 3,
        ];
        assert_eq!(
            decode(Service::Aliced, &frame),
            Ok(Some((
                "speech",
                json!({"source":"command","transcript":"stop"})
            )))
        );
        frame[23] = 0;
        assert_eq!(
            decode(Service::Aliced, &frame),
            Ok(Some((
                "speech",
                json!({"source":"voice","transcript":"hi"})
            )))
        );
        let partial = [
            0x1a, 21, 0x52, 9, 0x32, 7, b'c', b'o', b'm', b'm', b'a', b'n', b'd', 0x7a, 8, 0x0a, 4,
            b's', b't', b'o', b'p', 0x10, 0,
        ];
        assert_eq!(decode(Service::Aliced, &partial), Ok(None));
    }

    #[test]
    fn incomplete_duplicate_and_mixed_envelopes_do_not_bypass_validation() {
        for bytes in [
            vec![0xe8, 0x0f, 3],
            vec![0xe2, 0x0f, 0],
            vec![0xe2, 0x0f, 0, 0xe8, 0x0f, 0],
            vec![0xe2, 0x0f, 0, 0xe2, 0x0f, 0, 0xe8, 0x0f, 3],
            vec![0xe2, 0x0f, 0, 0xea, 0x0f, 0],
            vec![0x1a, 0, 0xe2, 0x0f, 0, 0xe8, 0x0f, 3],
            vec![0xe2, 0x0f, 1, 0, 0xe8, 0x0f, 3],
        ] {
            assert!(decode(Service::Aliced, &bytes).is_err(), "{bytes:x?}");
        }
    }

    #[test]
    fn malformed_wire_data_and_ambiguous_known_fields_are_rejected() {
        let malformed: &[&[u8]] = &[
            &[0],
            &[0x08, 0x80],
            &[0x12, 5, 1],
            &[0x09, 0],
            &[0x15, 0],
            &[0x0b],
            &[
                0x08, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 2,
            ],
            &[0x80, 0x80, 0x80, 0x80, 0x10, 0], // field number 2^29
            &[0x1a, 3, 0x12, 1, 0xff],
            &[0x1a, 4, 0x12, 0, 0x12, 0],
            &[0x1a, 0, 0x1a, 0],
            &[0x18, 1],
            &[0x1a, 2, 0x10, 1],
        ];
        for bytes in malformed {
            assert!(decode(Service::Aliced, bytes).is_err(), "{bytes:x?}");
        }
        // A matching field must not hide a malformed trailing field.
        assert!(decode(Service::Aliced, &[0x1a, 4, 0x12, 2, b'h', b'i', 0]).is_err());
        let mut too_many = Vec::new();
        for _ in 0..129 {
            too_many.extend_from_slice(&[0x08, 0]);
        }
        assert!(decode(Service::Aliced, &too_many).is_err());
        assert!(decode(Service::Aliced, &vec![0; 65537]).is_err());
    }
}
