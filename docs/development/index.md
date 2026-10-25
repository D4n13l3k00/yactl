# Разработка

Rust **1.99.1** закреплён в `rust-toolchain.toml` вместе с rustfmt, clippy и целью
AArch64 musl. При установленном rustup Cargo выбирает его автоматически; CI
использует тот же файл вместо плавающего stable. Cargo.lock фиксирует зависимости.
Разработка и обычные проверки адаптеров выполняются на Linux с симуляцией,
без физической колонки.

## Проверки

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
cargo build --locked
python3 tests/migration_cli.py target/debug/yactl
python3 tests/api_smoke.py target/debug/yactl
python3 tests/async_smoke.py target/debug/yactl
python3 tests/api_shutdown.py target/debug/yactl
python3 tests/lighting_smoke.py target/debug/yactl
python3 tests/clock_smoke.py target/debug/yactl
python3 tests/sensors_smoke.py target/debug/yactl
python3 tests/volume_smoke.py target/debug/yactl
python3 tests/events_smoke.py target/debug/yactl
python3 tests/firmware_smoke.py target/debug/yactl
python3 tests/input_smoke.py target/debug/yactl
python3 tests/smoke.py --binary target/debug/yactl
```

Для проверки отдельно собранного клиента подсветки:

```sh
python3 tests/midi_client_smoke.py target/debug/yactl /path/to/lighting-client
```

Репозитории собираются независимо. Этот дополнительный тест принимает готовый
бинарь клиента и не создаёт зависимости от его исходников.

[GitHub Actions](../../.github/workflows/checks.yml) выполняет Linux-проверки и сборку
статического AArch64-бинаря без упаковки. Проверка UPX и испытания на колонке
остаются частью подготовки релиза, а не заявленным результатом CI.

## Сборка для колонки

```sh
UPX_BIN=/path/to/upx sh build.sh
```

Цель указана в файле toolchain. Конфигурация использует `rust-lld` и статическую
линковку CRT. Релизный профиль оптимизирует размер, включает fat LTO, удаляет
символы и завершает процесс при панике. Скрипт собирает с lockfile, упаковывает
`dist/yactl` через `--best --lzma` и проверяет результат с `upx -t`.
UPX нужен для сборки, но не для запуска.

Испытывайте на колонке в отдельном каталоге. Сохраняйте SSH, установленные
настройки и посторонние загрузочные службы. Аппаратный тест требует явного
выбора адаптера и проверки восстановления. Наличие узла устройства в Linux
само по себе не подтверждает поддержку.
