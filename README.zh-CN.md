# 预测市场工具包

<div align="center">

<img width="820" alt="Polymarket 工具包 TUI" src="https://github.com/user-attachments/assets/b6c51ba1-14c6-4582-858c-e9441516dd1d" />
<img width="820" alt="预测市场工具包 仪表盘" src="https://github.com/user-attachments/assets/2ae5783d-be8e-458d-8da4-1ff82aada3db" />

### 平台无关的预测市场交易基础设施 — 任何带订单簿的市场

[![Rust](https://img.shields.io/badge/rust-1.70+-orange.svg?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Rust CI](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml/badge.svg)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml)
[![Stars](https://img.shields.io/github/stars/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=flat-square&color=6e40c9)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/stargazers)
[![License](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)
[![Tokio](https://img.shields.io/badge/async-tokio-blue.svg?style=flat-square)](https://tokio.rs/)
[![Venues](https://img.shields.io/badge/平台仓库-20-6e40c9.svg?style=flat-square)](#平台覆盖)

> **一套执行核心。一套风控层。覆盖所有平台。**
> 十款策略机器人共用同一套引擎与平台无关的适配层——接入新市场只需写**一个适配器**，而不是重建机器人。二十个平台已映射到该接口，各有专属仓库。

<br/>

[![在 Telegram 联系](https://img.shields.io/badge/💬_在_Telegram_联系-@tarpboi-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)
&nbsp;
[![PnL Profit 已上线](https://img.shields.io/badge/🚀_PnL_Profit-访问_pnlpro.fit-16a34a?style=for-the-badge)](https://pnlpro.fit)

**[快速开始](#-快速开始) • [策略](#策略) • [托管服务](#-托管与跟单交易当前已关闭) • [平台覆盖](#平台覆盖) • [引擎](#引擎) • [安全](#安全) • [联系方式](#联系方式)**

**🌐 Language / 语言 / Язык:** [English](README.md) • [简体中文](#预测市场工具包) • [Русский](README.ru.md)

</div>

---

## 🚀 快速开始

**无需安装 Rust 工具链。** 从[最新 release](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/releases/latest) 下载预编译二进制——Linux（x86_64 / aarch64）、macOS（Intel / Apple Silicon）、Windows——校验 SHA256，约一分钟即可运行：

```bash
tar -xzf polymarket-toolkits-<tag>-<target>.tar.gz
cd polymarket-toolkits-<tag>-<target>

cp config.yaml.example config.yaml   # 凭据；config.json 已随压缩包提供
./polymarket-toolkits run copy-trading   # 空跑：enable_trading 默认为 false
```

压缩包内含 `config.json`（公开设置）与 `config.yaml.example`（凭据）。运行 `./polymarket-toolkits --help` 查看完整命令，或不带子命令启动进入交互式 TUI。

想自己编译？`cargo install --git https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits`

<table>
<tr>
<td width="50%" valign="top">

### 🛠️ 自己运行机器人

开源引擎，你的密钥，你的钱包。

```bash
# 1. 克隆引擎（交易代码在这里）
git clone https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits
cd Prediction-Markets-Trading-Bot-Toolkits

# 2. 配置——从示例复制凭据
cp config.yaml.example config.yaml

# 3. 先空跑（不真正下单）
cargo run --release -- run copy-trading
```

每款机器人默认 `enable_trading: false`——完整执行链路会一直空跑，直到**你**亲手打开实盘。[各平台仓库](#平台覆盖)保存的是平台元数据，而不是可运行的机器人；请克隆本仓库。

</td>
<td width="50%" valign="top">

### 💬 先聊聊也可以

不确定哪种策略适合你的平台、资金规模或风险预算？直接问。

- 跟单交易在真实盘口上具体做什么，以及它的边界在哪里
- 在你打开 `enable_trading` 之前，空跑链路是如何工作的
- 哪种策略适合你的平台与资金规模，以及风控层如何限制风险

> ⏸️ **托管服务目前已关闭**——它此前以纸面交易测试版运行，从未经手真实资金。今天受支持的方式是自己运行。

**[→ 在 Telegram 联系](https://t.me/tarpboi)**

</td>
</tr>
</table>

---

## 数据一览

<div align="center">

[![Stars](https://img.shields.io/github/stars/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&logo=github&label=Stars&color=1f6feb)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/stargazers)
[![Forks](https://img.shields.io/github/forks/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&logo=github&label=Forks&color=1f6feb)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/forks)
[![CI](https://img.shields.io/github/actions/workflow/status/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/rust.yml?style=for-the-badge&logo=githubactions&logoColor=white&label=Build)](https://github.com/tarpboi/Prediction-Markets-Trading-Bot-Toolkits/actions/workflows/rust.yml)
[![License](https://img.shields.io/github/license/tarpboi/Prediction-Markets-Trading-Bot-Toolkits?style=for-the-badge&color=1f6feb)](LICENSE)

| 🎯 策略 | ⚙️ 引擎 | 🧪 空跑 |
|:---:|:---:|:---:|
| **10** | **Rust** | **全链路** |

*Star 与 Fork 数量使用实时徽章，而不是写死在 README 里的数字——它们跟随仓库实时变化，不会过时。没有虚假好评，没有挑拣过的 P&L。*

</div>

---

## 策略

十款策略，一套执行核心。每一款都围绕一个清晰、独立的市场优势，并且全部运行在同一套引擎之上：Polymarket CLOB 客户端、带 EIP-712 签名与真实空跑路径的下单执行器、风控护卫、持仓监控与存储、市场缓存，以及基于 Polygon 日志的链上采集。挑一个匹配你判断的优势上场——底层基础设施是共享的。

> 📦 每款机器人都运行在共享引擎与[安全层](#安全)之上，并完整支持空跑模式。[各平台仓库](#平台覆盖)保存的是平台元数据——交易代码在本仓库。

| # | 策略 | 一句话优势 | 关键规格 |
|---|------|-----------|----------|
| 1 | 🎯 **跟单交易** | 镜像已被证明拥有 alpha 的钱包 | 多钱包 · FAK/GTD · 熔断器 |
| 2 | ⚡ **BTC 5m / 15m / 1h 套利** | 短窗口 BTC 涨跌上的速度优势 | ~42ms 端到端 · FAK |
| 3 | 💰 **跨平台套利** | 锁价差，不锁方向 | Polymarket ↔ Kalshi ↔ PredictIt · 对冲双腿 |
| 4 | 🎯 **方向性套利** | 套利底仓（Up + Down < $1），再向更有优势的一侧倾斜 | 对冲底仓 · 仅限价单 |
| 5 | 📈 **价差耕作** | 一千次 0.5¢ 小胜复利成大数字 | 买卖价差捕获 · 单笔 P&L |
| 6 | 🏆 **体育执行** | 点击。成交。完成——不到 50ms | NBA / NFL / 足球 · &lt;50ms FAK |
| 7 | 🎯 **结算狙击** | 95¢ 近确定性 → 确定的 $1.00 派息 | 确定性扫描 · 持有至结算 |
| 8 | 📊 **订单簿失衡** | 信号本身就是订单簿——无需外部数据源 | 实时 OBI · 500ms 刷新 |
| 9 | 💰 **做市商** | 当庄家，不当赌客 | 双边 GTD · 库存倾斜 |
| 10 | ⚡ **链上鲸鱼信号** | 比公开仓位 API 早 3–30 秒 | Polygon 区块订阅 · ABI calldata 解码 |

<details>
<summary><b>几款旗舰优势的实际原理</b>（点击展开）</summary>

<br/>

**🎯 跟单交易 ——** 把机器人指向一个或多个链上战绩过硬的钱包，它会按你设定的规模镜像其成交，配有每钱包上限、FAK/GTD 订单类型，以及在异常爆发时暂停的熔断器。从任何拥有可核验链上战绩的钱包中挑选跟随对象。

**💰 跨平台套利 ——** 同一个现实问题常常同时挂在 Polymarket、Kalshi *和* PredictIt 上，价格略有差异。引擎会在各平台间**严格匹配同一份合约**（严格匹配——不制造虚假配对），并**仅在价差覆盖来回手续费时**才捕获它。跨平台市场大多是有效的，所以这是耐心游戏：它等待真正的错位，而不是硬凑交易。

**🎯 方向性套利 ——** 当 Yes + No 组合价低于 \$1 时买入（结构性套利底仓），再把额外仓位向更有上行空间的一侧倾斜。仅限价单、对冲底仓——用结构而非直觉来提升期望值。

**🎯 结算狙击 ——** 扫描近乎确定（如 95¢+）、市场实质已定但尚未派息的合约，持有到 \$1.00。高胜率、单笔低收益——靠成交量复利，而不是靠大幅波动。

**📊 订单簿失衡 ——** 无外部数据源、无预言机：信号本身就是订单簿。近盘口买卖深度的倾斜成为短线方向判断，每 500ms 刷新一次。

</details>

<div align="center">

💬 **想针对你的平台或资金规模详解某个策略？** → **[t.me/tarpboi](https://t.me/tarpboi)**

</div>

---

## 💼 托管与跟单交易（当前已关闭）

> ⏸️ **托管服务目前未开放。** 它此前以纸面交易测试版运行——资金为模拟，从未托管或经手真实资金；实盘交易一直以托管、安全审计与合规牌照为前置条件。**注册与定价现已关闭**，因此今天使用本项目的方式是用你自己的密钥自行运行（见[自己运行机器人](#-自己运行机器人)）。

如果你想聊聊未来重开时的托管方案，或如何自行规模化运行引擎，欢迎在 Telegram 上交流：

<div align="center">

[![在 Telegram 交流](https://img.shields.io/badge/交流机器人-Telegram-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)

</div>

---

## 平台覆盖

引擎在设计上**与平台无关**：任何提供订单簿或持仓数据的平台，都可以通过单一适配器接入——接入新市场意味着写一个适配器，而不是重建一个机器人。

二十个平台已映射到该接口，各有专属仓库。**Polymarket 是参考实现**，引擎正是基于它构建与测试；各平台仓库保存的是平台元数据，交易代码请克隆本仓库。

| 平台 | 类型 | 策略 |
|---|---|---|
| [**Polymarket**](https://github.com/tarpboi/Polymarket) | 去中心化（Polygon / USDC） | 参考实现——全部十款 |

### 传统 / 合规平台

| 平台 | 类型 |
|---|---|
| [**Kalshi**](https://github.com/tarpboi/Kalshi) | 受 CFTC 监管（美国） |
| [**PredictIt**](https://github.com/tarpboi/PredictIt) | 学术 / 美国政治 |
| [**Robinhood Predictions**](https://github.com/tarpboi/Robinhood-Predictions) | 券商集成 |
| [**Crypto.com Predictions**](https://github.com/tarpboi/Crypto.com-Predictions) | 加密集成 |
| [**OG.com**](https://github.com/tarpboi/OG.com) | 社交 / 多结果 |
| [**DraftKings Predictions**](https://github.com/tarpboi/DraftKings-Predictions) | 体育 |
| [**FanDuel Predicts**](https://github.com/tarpboi/FanDuel-Predicts) | 体育 |
| [**Fanatics Markets**](https://github.com/tarpboi/Fanatics-Markets) | 体育 / 娱乐 |
| [**Interactive Brokers ForecastTrader**](https://github.com/tarpboi/Interactive-Brokers-ForecastTrader) | 金融事件 |

### 加密 / 去中心化平台

| 平台 | 链 / 类型 |
|---|---|
| [**Limitless**](https://github.com/tarpboi/Limitless-Exchange) | 链上订单簿 |
| [**Drift BET**](https://github.com/tarpboi/Drift-BET) | Solana |
| [**Azuro**](https://github.com/tarpboi/Azuro) | 去中心化协议 |
| [**Augur**](https://github.com/tarpboi/Augur) | 以太坊 |
| [**Myriad Markets**](https://github.com/tarpboi/Myriad-Markets) | 加密 |
| [**Hedgehog Markets**](https://github.com/tarpboi/Hedgehog-Markets) | Solana / 社交 |
| [**Zeitgeist**](https://github.com/tarpboi/Zeitgeist) | Polkadot |
| [**Projection Finance**](https://github.com/tarpboi/Projection-Finance) | 波动率 / 模拟 |
| [**Better Fan**](https://github.com/tarpboi/Better-Fan) | 体育 / 电竞 |
| [**Manifold Markets**](https://github.com/tarpboi/Manifold-Markets) | 虚拟币 · 共识信号 |

> **希望优先支持某个平台？** 适配器开发由需求驱动——如果你希望某个平台被优先接入，[在 Telegram 联系我们](https://t.me/tarpboi)，它可以在队列中提前。

---

## 引擎

Rust 编写，基于 Tokio 异步，一套执行核心支撑所有策略与所有平台。适配层意味着接入新市场只需一个适配器——而不是一个新机器人。

### 性能

| | |
|---|---|
| **事件处理** | 每个事件 < 1ms |
| **下单执行** | 端到端 < 100ms |
| **仓位轮询** | 每个钱包约 200ms |
| **内存占用** | 基线约 50MB |
| **CPU** | 现代硬件下 < 5% |
| **并发** | 信号量限速（默认：25 请求 / 10 秒） |

---

## 安全

| | |
|---|---|
| **熔断器** | 在配置窗口内出现 N 笔连续大额成交后自动暂停 |
| **深度护卫** | 每笔下单前校验订单簿流动性 |
| **空跑模式** | 完整执行链路运行但不真正下单 |
| **下单底线** | 强制最小交易额，避免负 EV 微交易 |

熔断器在连续大额交易超过阈值，或订单簿深度低于下限时触发。一旦触发，执行将被屏蔽至冷却期结束。触发状态与冷却时间会被记录并显示在 TUI 中。

**建议：**

| 阶段 | 操作 |
|------|------|
| 初始部署 | 用 `enable_trading: false` 至少跑完一整轮观察 |
| 首次实盘 | 在信任信号前，将 `copy_percentage` 保持在 5–10% |
| 长期运行 | 关注熔断器触发事件——它们会暴露执行异常 |
| 生产环境 | 使用专用钱包，仅放入你计划部署的资金 |

---

## 联系方式

项目正在持续维护与开发中。无论你想**运行机器人**、**加入托管抢先体验候补名单**、请求**新的平台适配器**，还是想聊聊 Polymarket 工具与算法策略——都欢迎联系。

<div align="center">

[![在 Telegram 联系](https://img.shields.io/badge/💬_Telegram-@tarpboi-229ED9?style=for-the-badge&logo=telegram&logoColor=white)](https://t.me/tarpboi)

| 平台 | 链接 |
|------|------|
| **Telegram** | [t.me/tarpboi](https://t.me/tarpboi) |
| **讨论区** | [GitHub Discussions](../../discussions) |

*响应时间通常在数小时内。欢迎提问、反馈、平台请求与正经合作。*

</div>

---

## 免责声明

> 在预测市场交易涉及真实的财务风险。本软件按"原样"提供，不附带任何形式的担保或对结果的保证，且不构成投资建议。投入真实资金前，请务必先以 `enable_trading: false` 进行充分测试。**托管 / 跟单交易服务处于抢先体验测试阶段，运行于纸面模式（模拟资金）**——它不托管真实资金，任何实盘上线都将先行完成托管、审计与合规牌照。请确保遵守各平台的服务条款以及你所在司法管辖区的相关法规。

---

<div align="center">

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE)
[![Telegram](https://img.shields.io/badge/💬_Telegram-@tarpboi-229ED9?style=flat-square&logo=telegram&logoColor=white)](https://t.me/tarpboi)

**为 Polymarket、Kalshi、Limitless 等预测市场社区而构建**

[返回顶部](#预测市场工具包)

</div>

[机器人的力量](http://x.com/theparuchh/status/2053766299281416621)
