# Prediction Market Toolkits

<div align="center">

<img width="820" alt="Polymarket Toolkits TUI" src="https://github.com/user-attachments/assets/b6c51ba1-14c6-4582-858c-e9441516dd1d" />
<img width="820" alt="Панель Prediction Market Toolkits" src="https://github.com/user-attachments/assets/2ae5783d-be8e-458d-8da4-1ff82aada3db" />

### Торговая инфраструктура для рынков предсказаний, не зависящая от площадки — любой рынок со стаканом заявок

[![Rust](https://img.shields.io/badge/rust-1.70+-orange.svg?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Rust CI](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml/badge.svg)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml)
[![Stars](https://img.shields.io/github/stars/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=flat-square&color=6e40c9)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/stargazers)
[![License](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)
[![Tokio](https://img.shields.io/badge/async-tokio-blue.svg?style=flat-square)](https://tokio.rs/)
[![Venues](https://img.shields.io/badge/площадок-20-6e40c9.svg?style=flat-square)](#покрытие-площадок)

> **Одно ядро исполнения. Один слой риска. Все площадки.**
> Десять стратегических ботов используют один движок и стек адаптеров, не зависящий от площадки — добавить рынок значит написать **один адаптер**, а не пересобирать бота. Двадцать площадок сопоставлены с этим интерфейсом, у каждой свой репозиторий.

<br/>

[![Написать в Telegram](https://img.shields.io/badge/💬_Написать_в_Telegram-@tarpboi-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)
&nbsp;
[![PnL Profit — вживую](https://img.shields.io/badge/🚀_PnL_Profit-На_pnlpro.fit-16a34a?style=for-the-badge)](https://pnlpro.fit)

**[Быстрый старт](#-быстрый-старт) • [Стратегии](#стратегии) • [Сервис](#-управляемый-сервис-и-копи-трейдинг--сейчас-закрыт) • [Покрытие площадок](#покрытие-площадок) • [Движок](#движок) • [Безопасность](#безопасность) • [Контакты](#контакты)**

**🌐 Language / 语言 / Язык:** [English](README.md) • [简体中文](README.zh-CN.md) • [Русский](#prediction-market-toolkits)

</div>

---

## 🚀 Быстрый старт

**Rust-тулчейн не нужен.** Возьмите готовый бинарник из [последнего релиза](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/releases/latest) — Linux (x86_64 / aarch64), macOS (Intel / Apple Silicon), Windows — проверьте SHA256, и всё запустится примерно за минуту:

```bash
tar -xzf polymarket-toolkits-<tag>-<target>.tar.gz
cd polymarket-toolkits-<tag>-<target>

cp config.yaml.example config.yaml   # учётные данные; config.json уже в архиве
./polymarket-toolkits run copy-trading   # dry-run: enable_trading по умолчанию false
```

В архиве лежат `config.json` (публичные настройки) и `config.yaml.example` (учётные данные). `./polymarket-toolkits --help` покажет полный список команд, а запуск без подкоманды откроет интерактивный TUI.

Предпочитаете собрать сами? `cargo install --git https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits`

<table>
<tr>
<td width="50%" valign="top">

### 🛠️ Запустить ботов самому

Открытый движок, ваши ключи, ваш кошелёк.

```bash
# 1. Клонируйте движок (торговый код здесь)
git clone https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits
cd Prediction-Markets-Trading-Bot-Toolkits

# 2. Настройка — скопируйте пример учётных данных
cp config.yaml.example config.yaml

# 3. Сначала dry-run (реальных ордеров нет)
cargo run --release -- run copy-trading
```

У каждого бота `enable_trading: false` по умолчанию — весь путь исполнения работает в dry-run, пока **вы** сами это не измените. [Репозитории площадок](#покрытие-площадок) хранят метаданные площадок, а не рабочего бота; клонируйте этот.

</td>
<td width="50%" valign="top">

### 💬 Сначала можно обсудить

Не уверены, какая стратегия подходит вашей площадке, размеру капитала или риск-бюджету? Спросите.

- Что копи-трейдинг реально делает на живом стакане и где他 останавливается
- Как работает dry-run до того, как вы включите `enable_trading`
- Какая стратегия подходит вашей площадке и размеру капитала — и как её ограничивает риск-слой

> ⏸️ **Хостинговый сервис сейчас закрыт** — он работал как бета на бумажной торговле и никогда не касался реальных средств. Поддерживаемый путь сегодня — запустить самому.

**[→ Написать в Telegram](https://t.me/tarpboi)**

</td>
</tr>
</table>

---

## В цифрах

<div align="center">

[![Stars](https://img.shields.io/github/stars/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&logo=github&label=Stars&color=1f6feb)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/stargazers)
[![Forks](https://img.shields.io/github/forks/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&logo=github&label=Forks&color=1f6feb)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/forks)
[![CI](https://img.shields.io/github/actions/workflow/status/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/rust.yml?style=for-the-badge&logo=githubactions&logoColor=white&label=Build)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml)
[![License](https://img.shields.io/github/license/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&color=1f6feb)](LICENSE)

| 🎯 Стратегии | ⚙️ Движок | 🧪 Dry-run |
|:---:|:---:|:---:|
| **10** | **Rust** | **Каждый путь** |

*Звёзды и форки — живые бейджи, а не числа, вписанные в README: они следуют за репозиторием и не устаревают. Никаких фейковых отзывов и подобранного P&L.*

</div>

---

## Стратегии

Десять стратегий, одно ядро исполнения. Каждая нацелена на своё чётко очерченное рыночное преимущество, и все они работают на одном движке: клиент Polymarket CLOB, исполнитель ордеров с подписью EIP-712 и настоящим dry-run, риск-гард, монитор и хранилище позиций, кеш рынков и ончейн-приём из логов Polygon. Выбирайте преимущество под свою идею — инфраструктура под ним общая.

> 📦 Каждый бот работает на общем движке и [слое безопасности](#безопасность), с полной поддержкой dry-run. [Репозитории площадок](#покрытие-площадок) хранят метаданные площадок — торговый код здесь.

| # | Стратегия | Преимущество в одну строку | Ключевые параметры |
|---|-----------|----------------------------|--------------------|
| 1 | 🎯 **Копи-трейдинг** | Копируйте кошельки, уже доказавшие наличие альфы | Мультикошелёк · FAK/GTD · предохранитель |
| 2 | ⚡ **BTC арбитраж 5м / 15м / 1ч** | Скорость на коротких BTC Up/Down | ~42 мс end-to-end · FAK |
| 3 | 💰 **Межрыночный арбитраж** | Фиксируйте спред, а не направление | Polymarket ↔ Kalshi ↔ PredictIt · хеджированные ноги |
| 4 | 🎯 **Направленный арбитраж** | Арб-база (Up + Down < $1), затем перекос в сторону большего эджа | Хеджированная база · только лимит |
| 5 | 📈 **Сбор спреда** | Тысяча выигрышей по 0,5¢ складываются в одно число | Захват бид-аск · P&L по сделке |
| 6 | 🏆 **Спортивное исполнение** | Клик. Исполнено. Готово — менее 50 мс | NBA / NFL / футбол · &lt;50 мс FAK |
| 7 | 🎯 **Снайпер разрешения** | Околоуверенность 95¢ → гарантированная выплата $1.00 | Скан уверенности · удержание до разрешения |
| 8 | 📊 **Дисбаланс стакана** | Сигнал и есть стакан заявок — без внешних фидов | Живой OBI · обновление 500 мс |
| 9 | 💰 **Маркет-мейкинг** | Будьте казино, а не игроком | Двусторонний GTD · перекос инвентаря |
| 10 | ⚡ **Ончейн-сигнал китов** | На 3–30 с раньше публичного API позиций | Подписка на блоки Polygon · декод ABI calldata |

<details>
<summary><b>Как на самом деле работают флагманские эджи</b> (нажмите, чтобы развернуть)</summary>

<br/>

**🎯 Копи-трейдинг —** Наведите бота на один или несколько кошельков с доказанной ончейн-историей. Он копирует их сделки в вашем масштабе, с лимитами на кошелёк, ордерами FAK/GTD и предохранителем, который останавливает торговлю при аномальных всплесках. Выбирайте, за кем следовать, из любого кошелька с проверяемой ончейн-историей.

**💰 Межрыночный арбитраж —** Один и тот же реальный вопрос часто торгуется на Polymarket, Kalshi *и* PredictIt по слегка разным ценам. Движок сопоставляет один и тот же контракт между площадками (строгое сопоставление — без ложных пар) и захватывает разрыв **только когда он превышает комиссии на оба конца**. Межплощадочные рынки в основном эффективны, так что это игра на терпение: он ждёт реального рассинхрона, а не форсирует сделки.

**🎯 Направленный арбитраж —** Покупайте корзину Yes + No, пока она стоит меньше \$1 (структурная арб-база), затем перекашивайте дополнительный объём в сторону с большим апсайдом. Только лимит, хеджированная база — структура улучшает матожидание вместо ставки на догадку.

**🎯 Снайпер разрешения —** Сканирует околоуверенные контракты (например, 95¢+), где рынок фактически определился, но выплаты ещё нет, и удерживает до \$1.00. Высокий винрейт, низкая доходность на сделку — компаундится на объёме, а не на размахе.

**📊 Дисбаланс стакана —** Без внешних фидов и оракулов: сигнал и есть стакан. Перекос глубины бид/аск у лучших цен становится краткосрочным направленным сигналом, обновление каждые 500 мс.

</details>

<div align="center">

💬 **Хотите разбор стратегии под вашу площадку или капитал?** → **[t.me/tarpboi](https://t.me/tarpboi)**

</div>

---

## 💼 Управляемый сервис и копи-трейдинг — сейчас закрыт

> ⏸️ **Хостинговый сервис сейчас не работает.** Он запускался как бета на бумажной торговле — симулированные средства, без custody; реальных денег он никогда не касался, а живая торговля всегда была обусловлена custody, аудитом безопасности и лицензированием. **Регистрация и тарифы закрыты**, поэтому сегодня проект используют, запуская его самостоятельно со своими ключами (см. [Запустить ботов самому](#-запустить-ботов-самому)).

Если хотите обсудить управляемый вариант, когда он снова откроется, — или запуск движка на своих объёмах, — пишите в Telegram:

<div align="center">

[![Написать в Telegram](https://img.shields.io/badge/Обсудить_ботов-Telegram-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)

</div>

---

## Покрытие площадок

Движок **по замыслу** не зависит от площадки: любая платформа со стаканом заявок или лентой позиций подключается через один адаптер — добавить рынок значит написать адаптер, а не пересобирать бота.

Двадцать площадок сопоставлены с этим интерфейсом, у каждой свой репозиторий. **Polymarket — эталонная реализация**, на которой движок собран и протестирован; репозитории площадок хранят метаданные, поэтому за торговым кодом клонируйте этот репозиторий.

| Площадка | Тип | Стратегии |
|---|---|---|
| [**Polymarket**](https://github.com/tarpboi/Polymarket) | Децентрализованная (Polygon / USDC) | Эталонная реализация — все десять |

### Традиционные / регулируемые

| Площадка | Тип |
|---|---|
| [**Kalshi**](https://github.com/tarpboi/Kalshi) | Регулируется CFTC (США) |
| [**PredictIt**](https://github.com/tarpboi/PredictIt) | Академическая / политика США |
| [**Robinhood Predictions**](https://github.com/tarpboi/Robinhood-Predictions) | Брокерская интеграция |
| [**Crypto.com Predictions**](https://github.com/tarpboi/Crypto.com-Predictions) | Крипто-интеграция |
| [**OG.com**](https://github.com/tarpboi/OG.com) | Социальная / мультиисход |
| [**DraftKings Predictions**](https://github.com/tarpboi/DraftKings-Predictions) | Спорт |
| [**FanDuel Predicts**](https://github.com/tarpboi/FanDuel-Predicts) | Спорт |
| [**Fanatics Markets**](https://github.com/tarpboi/Fanatics-Markets) | Спорт / развлечения |
| [**Interactive Brokers ForecastTrader**](https://github.com/tarpboi/Interactive-Brokers-ForecastTrader) | Финансовые события |

### Крипто / децентрализованные

| Площадка | Сеть / тип |
|---|---|
| [**Limitless**](https://github.com/tarpboi/Limitless-Exchange) | Ончейн-стакан заявок |
| [**Drift BET**](https://github.com/tarpboi/Drift-BET) | Solana |
| [**Azuro**](https://github.com/tarpboi/Azuro) | Децентрализованный протокол |
| [**Augur**](https://github.com/tarpboi/Augur) | Ethereum |
| [**Myriad Markets**](https://github.com/tarpboi/Myriad-Markets) | Крипто |
| [**Hedgehog Markets**](https://github.com/tarpboi/Hedgehog-Markets) | Solana / социальная |
| [**Zeitgeist**](https://github.com/tarpboi/Zeitgeist) | Polkadot |
| [**Projection Finance**](https://github.com/tarpboi/Projection-Finance) | Волатильность / симуляции |
| [**Better Fan**](https://github.com/tarpboi/Better-Fan) | Спорт / киберспорт |
| [**Manifold Markets**](https://github.com/tarpboi/Manifold-Markets) | Игровые деньги · сигнал консенсуса |

> **Хотите, чтобы площадку сделали раньше?** Работа над адаптерами зависит от спроса — если вы торгуете на платформе, которую хотели бы видеть следующей, [напишите в Telegram](https://t.me/tarpboi), и она может подняться в очереди.

---

## Движок

Rust, асинхронность на Tokio, одно ядро исполнения за каждой стратегией и площадкой. Стек адаптеров означает, что новый рынок — это один адаптер, а не новый бот.

### Производительность

| | |
|---|---|
| **Обработка событий** | < 1 мс на событие |
| **Исполнение ордеров** | < 100 мс end-to-end |
| **Опрос позиций** | ~200 мс на кошелёк |
| **Память** | ~50 МБ базово |
| **CPU** | < 5% на современном железе |
| **Параллелизм** | Ограничение по семафору (по умолчанию: 25 запросов / 10 с) |

---

## Безопасность

| | |
|---|---|
| **Предохранитель (Circuit Breaker)** | Автостоп после N подряд крупных сделок в настраиваемом окне |
| **Защита глубины (Depth Guard)** | Проверяет ликвидность стакана перед каждым ордером |
| **Dry Run** | Полный путь исполнения проходит без реальных ордеров |
| **Минимум сделки (Trade Floor)** | Запрет микросделок с отрицательным EV |

Предохранитель срабатывает, когда число подряд идущих крупных сделок превышает заданный порог или глубина стакана падает ниже минимума. После срабатывания исполнение блокируется на время остывания. Состояние срабатывания и остывание логируются и видны в TUI.

**Рекомендации:**

| Этап | Действие |
|------|----------|
| Первичная настройка | Запустите с `enable_trading: false` на целую сессию |
| Первые реальные сделки | Держите `copy_percentage` на 5–10%, пока не доверитесь сигналу |
| Постоянно | Следите за срабатываниями предохранителя — они вскрывают аномалии исполнения |
| Продакшен | Выделенный кошелёк только с тем капиталом, который собираетесь задействовать |

---

## Контакты

Активно разрабатывается и поддерживается. Хотите **запустить ботов**, **встать в список раннего доступа к управляемому сервису**, запросить **новый адаптер площадки** или просто обсудить инструменты Polymarket и алгостратегии — пишите.

<div align="center">

[![Написать в Telegram](https://img.shields.io/badge/💬_Telegram-@tarpboi-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)

| Платформа | Ссылка |
|-----------|--------|
| **Telegram** | [t.me/tarpboi](https://t.me/tarpboi) |
| **Обсуждения** | [GitHub Discussions](../../discussions) |

*Обычно отвечаем в течение нескольких часов. Открыты к вопросам, отзывам, запросам площадок и серьёзному сотрудничеству.*

</div>

---

## Отказ от ответственности

> Торговля на рынках предсказаний сопряжена с реальным финансовым риском. Программное обеспечение предоставляется «как есть», без гарантий или обещания какого-либо результата. Это не финансовая консультация. Всегда тестируйте с `enable_trading: false` перед вводом реального капитала. **Управляемый сервис / копи-трейдинг находится в бете раннего доступа и работает в бумажном режиме (симулированные средства)** — он не хранит реальные деньги, а любой запуск реальной торговли будет сопровождаться надлежащим кастодианом, аудитом и лицензированием. Соблюдайте условия использования каждой площадки и применимые нормы вашей юрисдикции.

---

<div align="center">

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE)
[![Telegram](https://img.shields.io/badge/💬_Telegram-@tarpboi-229ED9?style=flat-square&logo=telegram&logoColor=white)](https://t.me/tarpboi)

**Создано для рынков предсказаний, включая Polymarket, Kalshi, Limitless и другие**

[Наверх](#prediction-market-toolkits)

</div>

[Power of Bot](http://x.com/theparuchh/status/2053766299281416621)
