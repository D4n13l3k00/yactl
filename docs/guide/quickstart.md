# Установка и первый запуск

yactl работает под Linux. Сборка для колонки — статический AArch64 musl-бинарь;
аппаратные адаптеры пока рассчитаны на Станцию Миди (`cucumber`). На обычном
Linux можно запускать менеджер сервисов и симуляцию без колонки.

## Локальный запуск

Установите rustup и выполните из каталога исходников:

```sh
cargo build --locked
mkdir -m 700 /tmp/yactl-demo
printf '%s\n' '{"version":1,"http":{"enabled":false}}' > /tmp/yactl-demo/settings.json
./target/debug/yactl --root /tmp/yactl-demo check
./target/debug/yactl --root /tmp/yactl-demo daemon --detach
./target/debug/yactl --root /tmp/yactl-demo health
./target/debug/yactl --root /tmp/yactl-demo services add example -- /bin/sleep 3600
./target/debug/yactl --root /tmp/yactl-demo services status example
./target/debug/yactl --root /tmp/yactl-demo services stop example
./target/debug/yactl --root /tmp/yactl-demo services remove example
./target/debug/yactl --root /tmp/yactl-demo shutdown
```

Если каталог уже существует, выберите другой. Здесь HTTP выключен: доступны
только приватные Unix-сокеты. Менеджер автоматически создаёт сервис `yactl-api`;
его остановка не останавливает остальные приложения. API-процесс запускается
асинхронно. Если первый запрос состояния пришёл до создания сокета, повторите его.

## Установка на Миди

Используйте [инструкцию по сборке](../development/index.md#сборка-для-колонки) или проверенный
релиз для AArch64 musl. Init-скрипт рассчитан на следующие пути:

| Файл | Путь |
| --- | --- |
| Бинарь | `/data/.yactl/yactl` |
| Каталог состояния | `/data/.yactl` |
| Конфигурация | `/data/.yactl/settings.json` |
| Автозапуск | `/etc/init.d/S99yactl` |

Режим доступа к бинарю — 0755, к каталогу состояния — 0700, к конфигурации — 0600.
[Пример для Миди](../../deploy/settings-midi.example.json) включает аппаратные
адаптеры и три устройства ввода. Перед применением проверьте выбор устройств
для своей колонки. Без конфигурации аппаратные адаптеры выключены.

Проверьте настройки и запуск:

```sh
/data/.yactl/yactl check
/data/.yactl/yactl daemon --detach
/data/.yactl/yactl health
/data/.yactl/yactl capabilities
```

[Init-скрипт](../../deploy/S99yactl) поддерживает `start`, `stop`, `restart` и `status`.
Устанавливайте его с режимом 0755 после успешного ручного запуска и остановки.
Корневая файловая система прошивки может быть доступна только для чтения:
меняйте режим монтирования лишь на время установки скрипта и возвращайте исходный.
Сохраняйте существующие загрузочные скрипты и доступ по SSH. Перезапуск службы
через скрипт не заменяет проверку полной перезагрузки колонки.

## Fish

Скопируйте [настройку PATH](../../deploy/yactl.fish) в
`~/.config/fish/conf.d/yactl.fish` и создайте автодополнение:

```fish
mkdir -p ~/.config/fish/conf.d ~/.config/fish/completions
cp deploy/yactl.fish ~/.config/fish/conf.d/yactl.fish
/data/.yactl/yactl completions fish > ~/.config/fish/completions/yactl.fish
source ~/.config/fish/conf.d/yactl.fish
```

Команда `cp` рассчитана на запуск из исходников; на колонку сначала перенесите
этот файл. Новые сессии fish загрузят его автоматически. В уже открытой сессии
используйте `source`. Сам бинарь остаётся в `/data/.yactl`.

## Управление программой

Программа должна работать на переднем плане, чтобы менеджер отслеживал её процесс.

```sh
yactl services add my-agent -- /data/opt/local/bin/my-agent
yactl services logs my-agent --follow
yactl services restart my-agent
yactl services disable my-agent --now
```

`add` запускает сервис и включает его старт вместе с менеджером, если не указан
`--disabled`. Команда `enable` не устанавливает системный init-скрипт.
`stop` меняет текущее намерение запуска. Записи сервисов хранятся отдельно от
общих настроек. Параметры перезапуска, логов и тайм-аутов описаны в
`yactl services add --help`.

Перед обновлением сохраните настройки и записи сервисов. yactl не преобразует
пакеты другого менеджера и не останавливает его автоматически.
