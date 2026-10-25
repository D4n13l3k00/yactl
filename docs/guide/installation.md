# Установка из CI

Пока нет опубликованного стабильного релиза, можно установить сборку из GitHub
Actions. Это временный способ для **Станции Миди с AArch64 Linux и доступом root**.
Не используйте этот бинарь на других архитектурах.

## Скачать сборку

1. Откройте вкладку **Actions** репозитория yactl и workflow **Checks**.
2. Выберите успешный запуск нужной ветки и проверьте коммит. Для обычной установки
   используйте доверенную основную ветку, а не чужой pull request.
3. В разделе **Artifacts** скачайте `yactl-aarch64-unknown-linux-musl`.
   GitHub может потребовать вход. Артефакт хранится 14 дней после сборки.
4. Распакуйте ZIP от GitHub. В нём будут архив программы и `SHA256SUMS`.
   В каталоге с ними выполните:

```sh
sha256sum -c SHA256SUMS
tar -xzf yactl-aarch64-unknown-linux-musl.tar.gz
```

Внутри: `yactl`, `LICENSE`, `INSTALL.md` и каталог `deploy` с init-скриптом,
примером настроек Миди и настройкой fish. Бинарь статический и упакован UPX;
Rust, UPX и системная musl на колонке для запуска не нужны.

## Перенести на колонку

Передайте бинарь и каталог `deploy` по SSH/SCP или ADB в отдельный временный
каталог на колонке, например `/tmp/yactl-install`. Не заменяйте файлы прошивки.
Далее команды выполняются **на колонке от root в `sh`**; если входная оболочка
fish, сначала запустите `sh`.

Для обновления сначала сохраните старые файлы и остановите установленный yactl:

```sh
umask 077
if [ -d /data/.yactl ]; then
    backup=$(mktemp -d /data/yactl-backup.XXXXXX)
    if [ -x /data/.yactl/yactl ]; then
        /data/.yactl/yactl shutdown || exit 1
    fi
    cp -a /data/.yactl "$backup/state"
    if [ -f /etc/init.d/S99yactl ]; then
        cp /etc/init.d/S99yactl "$backup/S99yactl"
    fi
    echo "Backup: $backup"
fi
mkdir -p /data/.yactl
chmod 700 /data/.yactl
cp /tmp/yactl-install/yactl /data/.yactl/yactl.new
chmod 755 /data/.yactl/yactl.new
mv /data/.yactl/yactl.new /data/.yactl/yactl
```

Если yactl ещё не установлен, ветка резервирования пропускается. Если остановка
старой службы завершилась ошибкой, разберитесь с ней до замены. Настройки и
записи сервисов при обновлении не перезаписывайте.

## Конфигурация и ручной запуск

Для первого запуска достаточно настроек по умолчанию: оборудование выключено,
HTTP слушает только `127.0.0.1:8098`. Если нужен аппаратный API Миди, скопируйте
пример **только при отсутствии своего файла**:

```sh
if [ ! -e /data/.yactl/settings.json ]; then
    cp /tmp/yactl-install/deploy/settings-midi.example.json /data/.yactl/settings.json
    chmod 600 /data/.yactl/settings.json
fi
/data/.yactl/yactl check
/data/.yactl/yactl daemon --detach
/data/.yactl/yactl health
/data/.yactl/yactl capabilities
```

Пример включает RGB, часы, датчики, громкость, выбранные устройства ввода и
поиск aliced. Сверьте его с [настройкой](configuration.md). API запускается
асинхронно: при слишком раннем запросе повторите `health` после старта.

## Автозапуск

После успешного ручного запуска установите `S99yactl`. На тестовой Миди корневая
файловая система была доступна только для чтения. Следующий блок проверяет режим
и возвращает его после установки; остальные init-скрипты и SSH он не меняет.

```sh
set -eu
root_options=$(awk '$2 == "/" {print $4}' /proc/mounts)
restore_ro=no
case ",$root_options," in
    *,ro,*) restore_ro=yes ;;
esac
if [ "$restore_ro" = yes ]; then
    trap 'mount -o remount,ro /' EXIT
    mount -o remount,rw /
fi
cp /tmp/yactl-install/deploy/S99yactl /etc/init.d/.S99yactl.new
chmod 755 /etc/init.d/.S99yactl.new
mv /etc/init.d/.S99yactl.new /etc/init.d/S99yactl
sync
if [ "$restore_ro" = yes ]; then
    mount -o remount,ro /
    trap - EXIT
fi
/etc/init.d/S99yactl restart
/etc/init.d/S99yactl status
/data/.yactl/yactl health
```

Штатный init тестовой Миди вызывает скрипты `S??*` при загрузке. Здесь бинарь,
конфигурация и состояние находятся в `/data/.yactl`; скрипт — в `/etc/init.d`.
Перезапуск через init проверен, **полная перезагрузка колонки пока не проверялась**.
Не перезагружайте устройство, пока ручной запуск и доступ по SSH не подтверждены.

## Команда и автодополнение в fish

В fish выполните:

```fish
mkdir -p ~/.config/fish/conf.d ~/.config/fish/completions
cp /tmp/yactl-install/deploy/yactl.fish ~/.config/fish/conf.d/yactl.fish
/data/.yactl/yactl completions fish > ~/.config/fish/completions/yactl.fish
source ~/.config/fish/conf.d/yactl.fish
yactl status
```

Новые сессии загрузят PATH и автодополнение автоматически.

## Если запуск не удался

Проверьте `yactl check`, `yactl status` и `yactl services logs yactl-api`.
Не удаляйте каталог состояния для исправления ошибок конфигурации.
Для отката остановите менеджер, верните сохранённый бинарь, совместимые настройки
и init-скрипт, затем запустите и проверьте состояние. Не восстанавливайте старый
каталог поверх работающей службы. [Ограничения поддержки](../overview/support.md) применяются
и к сборкам CI.
