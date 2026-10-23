# 在程序中嵌入 lode

[English](embedding.md) · **中文**

大多数用户直接运行 `lode` 二进制。本文面向另一种用法:一个把 lode 作为库链接进来的 Rust 程序。
可以嵌入的东西有两类,彼此独立:

| 你想要 | 用什么 | crate |
|---|---|---|
| 程序用已签名的发布**替换自己的可执行文件** | [`SelfUpdate`](#2-更新自己的可执行文件) | `lode-core` |
| 程序把**另一个应用**作为受监管的子进程来运行和更新 | [`Engine` + `serve_embedded`](#3-监管另一个应用) | `lode-core`、`lode-supervisor` |

两者在 [`examples/embed`](../examples/embed) 里都有完整、可运行的示例程序。

## 1. 准备

这些 crate 没有发布到 crates.io(那里的同名包属于无关项目),请依赖发布 tag:

```toml
[dependencies]
lode-core       = { git = "https://github.com/bkhq/lode", tag = "v0.2.0" }
lode-supervisor = { git = "https://github.com/bkhq/lode", tag = "v0.2.0" }   # 仅在监管子进程时需要
```

### 进程级副作用需要显式开启

`lode` 二进制会安装 TLS 加密 provider、tracing 订阅器、panic hook 并禁用 core dump。
作为库使用时,这些一个都不会自动发生:

```rust
lode_core::InitOptions::new()
    .crypto_provider(true)     // 第一次 HTTPS 请求之前必须有
    .install();
```

| 选项 | 作用 | 什么时候开 |
|---|---|---|
| `crypto_provider(true)` | 安装进程级 rustls provider | 除非程序自己安装了 provider,否则都要开。没有 provider 时第一次 HTTPS 请求就会失败。 |
| `log_level("info")` / `logging(Some(..))` | 安装 lode 的全局 `tracing` 订阅器 | 程序自己没有订阅器时。已有订阅器就不要开,lode 的事件会进入你的订阅器。 |
| `panic_hook(true)` | 通过 `tracing` 记录 panic | 想用 lode 的 hook 时。 |
| `suppress_core_dumps(true)` | 把 core dump 上限设为 0 | 进程里有密钥、不希望被 dump 时。 |

`install()` 可以重复调用。

### 错误

`lode-core` 返回 `lode_core::Error`,它的变体标明子系统(`Config`、`Http`、`Manifest`、
`Verify` ……)并带一条消息。`SelfUpdate` 有自己更细的错误枚举,见
[错误以及它们留下的状态](#错误以及它们留下的状态)。

## 2. 更新自己的可执行文件

`lode_core::self_update::SelfUpdate` 下载程序自身的一个发布、完成校验,然后放到正在运行的
可执行文件的位置上。进程在重启之前继续运行旧代码;什么时候重启由你决定。

```rust
use lode_core::RequireManifest;
use lode_core::self_update::{self, Outcome, SelfUpdate, Source};

// 启动时取一次:更新之后,正在运行的文件已经不在原路径上了。
let exe = self_update::current_exe()?;
self_update::remove_replaced(&exe);          // Windows:清理上次更新留下的旧文件
lode_core::InitOptions::new().crypto_provider(true).install();

let update = SelfUpdate::new("myapp", Source::GitHub("owner/myapp".into()), env!("CARGO_PKG_VERSION"))
    .asset("myapp-linux-x64.tar.gz")
    .trusted_keys(["<key_id>:<base64-public-key>"])
    .exe(exe);

if let Outcome::Installed { version, .. } = update.apply()? {
    println!("已安装 {version},重启后生效");
}
```

### 可配置项

| 方法 | 必填 | 含义 |
|---|---|---|
| `SelfUpdate::new(app, source, current_version)` | 是 | `app` 是发布签名时绑定的应用名(见[发布](#发布一个程序能安装的版本))。`current_version` 是当前运行的版本,通常是 `env!("CARGO_PKG_VERSION")`。 |
| `.asset(name)` | 是 | 本平台对应的发布文件。扩展名决定格式:`.tar.gz` / `.tgz`、`.zip`、`.gz`,或原始文件。 |
| `.trusted_keys([..])` | 是 | 发布必须由其中之一签名的 `<key_id>:<公钥>` 条目。**请编译进程序。** 从文件或配置里读的密钥,能改配置的人就能改。 |
| `.binary(name)` | 否 | 可执行文件在包内的名字。默认是 `app`,Windows 上加 `.exe`。 |
| `.exe(path)` | 否 | 要替换的文件。默认是调用时正在运行的可执行文件。长期运行的程序应在启动时解析一次并传入。 |
| `.alias(path)` | 否 | 需要一并替换的第二份拷贝(Windows 上第二个名字是拷贝而不是软链)。不存在的路径会被跳过。 |
| `.version(id)` | 否 | 安装指定版本,而不是通道的最新版。这也是降级的唯一途径。 |
| `.channel(name)` | 否 | 默认 `stable`。GitHub 源下,其他通道跟随最新的 pre-release。 |
| `.header("Name: value")` | 否 | 私有源的请求头。`${VAR}` 在发请求时从环境变量取。只发给源自己的主机。 |
| `.require_manifest(policy)` | 否 | `RequireManifest::Signed` 或 `Fresh`,见[冻结攻击](#冻结攻击与-require_manifest)。 |

### 源

| 源 | 读取什么 | 说明 |
|---|---|---|
| `Source::GitHub("owner/repo")` | GitHub Releases API | 版本号是去掉前导 `v` 的 release tag。签名是名为 `<asset>.sig` 的 release 资产。未鉴权调用每个 IP 每小时 60 次;不够用时用 `.header("Authorization: Bearer ${GITHUB_TOKEN}")` 传 token。 |
| `Source::Manifest(url)` | 一份 `lode/v1` 清单 | 自己的服务器,或 [lode-server](https://github.com/bkhq/lode-server) 注册中心。可以带通道、更新说明、发布时间和清单签名。见 [source-adapters.zh-CN.md](source-adapters.zh-CN.md)。 |

除回环地址外,明文 `http` 一律拒绝。

### 三个步骤

`apply()` 等于先 `fetch()` 再 `install()`。需要在两者之间做事时,自己分开调用。

| 调用 | 做什么 | 是否改动已安装的文件 |
|---|---|---|
| `check()` | 读取源,返回一次更新会安装的发布 —— `Available { version, entry }` —— 或 `None`。 | 否 |
| `fetch()` | 下载资产、校验摘要和签名、解压到临时目录,返回 `Fetched`;已是最新时返回 `None`。 | 否 |
| `install(&fetched)` | 把新二进制复制到可执行文件旁边,用 `--version` 运行并要求它打印出该发布的版本号,然后放到可执行文件的位置上。 | 是 |

**你的程序必须响应 `--version`**,输出中要包含以空白分隔的版本号(`myapp 1.4.2` 即可)。
`install` 靠它在替换一个能用的二进制之前,拦住为其他平台构建的包或标错版本的包。

`check()` 返回清单里的版本条目,所以 `--check` 命令可以展示更新内容:

```rust
if let Some(available) = update.check()? {
    println!("{} ({})", available.version, available.entry.published_at.as_deref().unwrap_or("?"));
    println!("{}", available.entry.notes.as_deref().unwrap_or(""));
}
```

`notes` 和 `published_at` 不在任何签名范围内,请当作不可信文本处理:输出到终端前去掉控制字符,
放进 HTML 时转义。

### 随可执行文件一起发布的文件

`Fetched` 在被释放之前,持有整个已校验并解压的资产:

```rust
if let Some(fetched) = update.fetch()? {
    // 例如必须放在可执行文件旁边的 DLL
    install_companions(fetched.dir())?;
    update.install(&fetched)?;
}
```

| 访问器 | 得到 |
|---|---|
| `fetched.dir()` | 资产解压到的目录 |
| `fetched.binary()` | 其中的新可执行文件 |
| `fetched.version()` / `fetched.entry()` | 版本号及其清单条目 |

`Fetched` 被释放时会删除临时目录,需要的文件要先复制出来。

希望自己替换可执行文件的程序,在 `fetch()` 之后停下、不调用 `install()` 即可。此后的事都归它自己:
确认新二进制能启动、在 Windows 上替换运行中的可执行文件、失败后的恢复。不要直接在临时目录里运行新二进制
来做检查 —— 系统临时目录可能以 `noexec` 挂载;应当像 `install()` 那样先复制到可执行文件旁边。

### 校验了什么

每次更新都会做,无法关闭:

1. **摘要。** 下载内容的 sha256 必须与源声明的一致。
2. **签名。** 该摘要、应用名、资产文件名和版本号必须由 `trusted_keys` 中的某把密钥签名。未签名的发布会被拒绝。
3. **顺序。** 跟随通道最新版时只会前进:比 `current_version` 旧的最新版会被拒绝,无法比较的(双方不都是
   semver)也会被拒绝。`.version(id)` 可以有意安装任意版本。
4. **能运行。** 见上面的 `--version`。

源无法让你的程序安装一个你的密钥没有签过的东西,也无法把它换到你的密钥没有签过的名字或版本上。

### 冻结攻击与 `require_manifest`

上面的校验挡不住一种情况:源一直返回一份旧清单。里面每个签名都有效,你的程序却永远停在旧版本上,
而逐个资产的签名发现不了这一点。带已签名有效期的已签名清单可以:

```rust
.require_manifest(RequireManifest::Fresh)
```

| 策略 | 对清单的要求 |
|---|---|
| `Off`(默认) | 无。签名或有效期存在时会校验,但不强制要求。 |
| `Signed` | 必须带有对通道和资产摘要的有效签名。 |
| `Fresh` | 在 `Signed` 之上,还必须带有尚未到期的已签名有效期。 |

这需要使用 `Source::Manifest`,并且发布者对清单签名(`lode-cli manifest-sign --expires-in 30d`),
还要在到期之前重新签名。GitHub 源没有可签名的清单:把它和 `Signed` 或 `Fresh` 组合是配置错误。

有效期与本机时钟比较,没有宽限:到期的那一秒起清单就被拒绝。发布者应当留足余量提前续签。

### Windows

Windows 上正在运行的可执行文件不能被覆盖或删除,只能改名。因此 `install()` 会先把旧文件改名挪开
(变成旁边一个隐藏的 `.<name>.old-…`),再把新文件移到原位,失败时把旧文件改回去。

- 旧文件会保留到没有进程使用它为止。启动时调用 `self_update::remove_replaced(&exe)` 来删除。
- 程序以两个名字安装时,用 `.alias(path)` 指出另一个;这里没有软链可以跟随。
- 安装在需要管理员权限的目录(`Program Files`)时,普通用户的改名会失败,可执行文件保持原样。

Unix 上的替换是一次原子 `rename`,运行中的进程继续持有旧文件。

### 错误以及它们留下的状态

`SelfUpdate` 的每个方法都返回 `SelfUpdateError`。对它做 match 来选择文案;不关心细节时,
它可以通过 `?` 转成 `lode_core::Error`。

| 变体 | 含义 | 已安装的文件 |
|---|---|---|
| `Config` | 没有可信密钥、没有资产名、不可能的组合 | 未改动 |
| `Source` | 源连不上,或返回内容无法读取 | 未改动 |
| `Verify` | 摘要或签名校验失败;在 `require_manifest` 下清单未签名或已过期 | 未改动 |
| `Local` | 临时目录或解压失败 | 未改动 |
| `Older { latest, current }` | 最新发布比当前运行的版本旧 | 未改动 |
| `Unordered { latest, current }` | 两个版本号不都是 semver | 未改动 |
| `MissingBinary { asset, binary }` | 资产里没有可执行文件 | 未改动 |
| `NotRunnable { path, source }` | 新二进制无法启动 | 未改动 |
| `WrongVersion { expected, reported }` | 新二进制报告的是另一个版本 | 未改动 |
| `Replace { path, source }` | 可执行文件无法替换 | 未改动 |
| `Alias { path, source }` | 可执行文件已替换,它的某份拷贝没有 | **两个版本并存** |
| `Displaced { path, moved_to, source }` | Windows:旧可执行文件已被挪开,新的没能放到位,旧的也挪不回去 | **`path` 处没有文件;旧文件在 `moved_to`** |

只有最后两个变体的 `error.is_untouched()` 为 `false`。遇到 `Alias` 时重试更新;遇到 `Displaced` 时
把 `moved_to` 改名回 `path`。

该枚举标了 `#[non_exhaustive]`:match 时保留一个兜底分支。

### 发布一个程序能安装的版本

签名绑定了应用名、资产文件名和版本号,这三项都必须与程序请求的一致。

```bash
lode-cli keygen --out publisher            # 只做一次;把打印出的 trustedKeys 条目编译进程序
lode-cli sign myapp-linux-x64.tar.gz --app myapp --version 1.4.2 --key publisher.key
```

- **GitHub Releases**:上传资产,以及一个内容为所打印 `sig` 的 `myapp-linux-x64.tar.gz.sig` 文件。
  release tag 为 `v1.4.2` 或 `1.4.2`。
- **清单**:`lode-cli manifest … --into manifest.json`;如果程序设置了 `require_manifest`,再执行
  `lode-cli manifest-sign --into manifest.json --key publisher.key --expires-in 30d`。
- **lode-server 注册中心**:`lode-cli publish`。

`--app` 必须是传给 `SelfUpdate::new` 的 `app`,文件名必须是 `.asset(...)`,版本号必须是新二进制
对 `--version` 的输出。细节见 [source-adapters.zh-CN.md](source-adapters.zh-CN.md) 和
[integration.zh-CN.md](integration.zh-CN.md)。

## 3. 监管另一个应用

这种用法下你的程序是宿主,lode 的循环运行在它内部:把应用作为子进程启动、带退避地重启、应用更新、
回滚起不来的版本。这就是 `lode` 二进制做的事,只是去掉了宿主想自己掌握的那部分。

```rust
use lode_core::{Config, Engine, InitOptions};
use lode_supervisor::{SuperviseOptions, serve_embedded, signal_channel};

InitOptions::new().crypto_provider(true).install();
let cfg = Config::from_toml(include_str!("worker.toml"))?;

let (signals, mut source) = signal_channel();
let supervised = cfg.clone();
let supervisor = std::thread::spawn(move || {
    serve_embedded(supervised, &mut source, SuperviseOptions::host_owned())
});

let engine = Engine::new(cfg);
engine.restart()?;                 // 请求循环重启应用
signals.send(15)?;                 // SIGTERM:优雅停止
let exit = supervisor.join().expect("supervise thread")?;
```

### 配置

`Config` 就是 `lode.toml` 里的内容。可以从 TOML 文本构建,也可以用 builder:

```rust
let cfg = Config::from_toml(toml_text)?;                       // lode.toml 的全部键

let cfg = Config::builder()
    .app("worker")
    .dir("/var/lib/myhost/worker")
    .manifest("https://lode.bk.io/r/<namespace>/<slug>/manifest.json")
    .asset("worker-linux-x64.tar.gz")
    .run("./worker")
    .build()?;
```

builder 覆盖源、资产、通道、策略、版本固定、启动命令和 `[report]`。`[trust]`、`[http]`、
`[supervise]`、`[runtime]` 请用 `from_toml`,键名与 [`lode.example.toml`](lode.example.toml) 一致。
两者都不读取文件,也不读取 `LODE_*` 环境变量。

### 状态上报

配置了 `[report]` 端点后,循环会通过 OTLP/HTTP 推送应用状态,与 `lode` 二进制完全一致:
启动时、每个 `interval` 以及停止前各导出一次指标(`lode.app.status`、重启次数、运行时长、
可用版本与最近可用版本、资源版本),并在发生时发送 `lode.*` 生命周期事件(`started`、
`updated`、`rolled_back`、`paused`、`check.failed`、`stopping`)。`service.name` 是被监管的应用,
`process.pid` 是宿主进程本身。

```rust
let cfg = Config::builder()
    .app("worker")
    .manifest("https://lode.bk.io/r/<namespace>/<slug>/manifest.json")
    .report_endpoint("https://lode.bk.io/r/otlp")   // 任意 OTLP/HTTP 基址
    .report_header("Authorization: Bearer ${LODE_REGISTRY_TOKEN}")
    .report_attribute("deployment.environment.name", "prod")
    .report_interval(300)                           // 默认值
    .build()?;
```

这些请求头只发往上报端点,`${ENV}` 在每次请求时展开。端点必须是 https(或回环 http),
除非设置了 `[http].allow_insecure`。上报在独立线程中运行,不会阻塞循环;`[report]` 与 `[trust]`
一样在循环生命周期内固定。各项导出内容见 [operations.zh-CN.md](operations.zh-CN.md#状态上报)。

### 谁负责什么

`serve_embedded(cfg, &mut signals, options)` 会阻塞到应用停止,所以要放在单独的线程里。

| | `SuperviseOptions::host_owned()` | `SuperviseOptions::owned()` |
|---|---|---|
| 信号处理 | 宿主负责,通过 `signal_channel()` 喂给循环。 | lode 负责:传入 `OwnedSignalSource`。 |
| 子进程 subreaper | 不设置 | 设置(用于作为 PID 1 的进程) |
| 单实例锁(`lode.pid`) | 不获取 | 获取 |

使用 `host_owned()` 时 lode 不改动任何进程级状态。通道是信号进入循环的唯一途径:`SIGTERM`(15)、
`SIGINT`(2)或 `SIGQUIT`(3)会优雅停止应用;`[signals].forward` 中的信号(默认是一组常用信号)
会转发给应用;其余的被忽略。

### 驱动循环

`Engine` 是 `lode-cli` 所基于的门面。它的调用并不直接与循环通信:它们把请求写进数据目录里的
`state.json`,循环在大约一秒内取走。另一个进程也可以发出同样的调用。

| 调用 | 做什么 |
|---|---|
| `check()` | 读取源:通道当前指向的版本。不下载任何东西。 |
| `install(None)` / `install(Some(v))` | 下载、校验、安装并激活最新版或 `v`。 |
| `rollback(None)` / `rollback(Some(v))` | 切换到上一个已知良好的版本,或切换到 `v`。 |
| `restart()` | 重启应用。 |
| `resolve_target()` | 现在启动的话会是哪个已安装版本。 |
| `status()` / `versions()` | 向 stdout 打印 `lode-cli status` / `versions` 的输出。 |

想读取状态而不是打印,用 `lode_core::state::read(&dir.join("state.json"))`:其中有 `current`、
`last_good`、`available`、`status`、`pid`、`last_error` 和发布历史 `history`。

应用也可以通过同一个文件参与自己的更新 —— 上报就绪、请求检查、控制切换时机;见
[integration.zh-CN.md](integration.zh-CN.md) 和 [`sdks/`](../sdks) 里的 SDK。

### 被监管应用的发布

把配置指向一个源,并要求签名:

```toml
[update]
manifest = "https://lode.bk.io/r/<namespace>/<slug>/manifest.json"   # 或:github = "owner/repo"
asset    = "worker-linux-x64.tar.gz"
policy   = "auto"                    # off | check | auto

[trust]
require_signature = "enforce"
trusted_keys = ["<key_id>:<base64-public-key>"]
```

没有任何已安装版本时,循环会先下载通道的最新版再启动应用。测试和演示可以用
`lode_core::install::seed_local` 把本地文件安装为一个版本,不需要任何源。

## 4. 平台

| | Linux、macOS | Windows |
|---|---|---|
| `SelfUpdate` | 支持 | 支持,见 [Windows](#windows) |
| `Engine` | 支持 | 支持 |
| 监管循环 | 进程组和信号 | Job Object 和控制台事件;没有信号可转发 |

## 5. 示例

[`examples/embed`](../examples/embed) 是一个独立的小 crate,包含两个都可以离线运行的程序:

```bash
cargo run --manifest-path examples/embed/Cargo.toml --bin self_update
cargo run --manifest-path examples/embed/Cargo.toml --bin supervise
```

- `self_update` 构建并签名一个发布,在回环地址上提供服务,然后依次演示 `check`、`fetch`、附带文件、
  `install` 以及一次被拒绝的更新。
- `supervise` 在循环下运行一个 worker,切换它的版本、重启它、再停止它。
