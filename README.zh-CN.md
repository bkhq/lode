# lode

[English](README.md) · **中文**

> 通用的**「校验 · 启动 · 更新」加载器**:一个极小的静态 Rust 二进制,校验已打包应用的
> **完整性**与**发布者身份**,启动、监督并热更新它。把它打进通用镜像一次 —— 换应用只是换一份
> manifest,**永不重建镜像**。

- **镜像:** `docker.io/bkhq/lode`([Docker Hub](https://hub.docker.com/r/bkhq/lode))
- **二进制:** Linux(x86_64 / aarch64,musl 静态)、macOS(x86_64 / arm64)和 Windows(x86_64) —— [Releases](https://github.com/bkhq/lode/releases);就地升级用 `lode-cli self-update`
- **平台:** Linux、macOS 和 Windows。在 Windows 上 app 运行在 Job Object 里,用 `CTRL_BREAK` 请求停止;信号转发仅 Unix 可用。→ [运维](docs/operations.zh-CN.md)

## 从这里开始 —— 按角色

| 你是… | 你想… | 去看 |
|---|---|---|
| **运维** | 在容器里跑一个应用并保持更新 | [快速开始](#快速开始) + [`lode.example.toml`](docs/lode.example.toml) + [运维](docs/operations.zh-CN.md) |
| **应用作者** | 让你的应用可被 lode 更新 | [集成 §2 — 应用契约](docs/integration.zh-CN.md) |
| **发布方** | 打包、签名并发布一个版本 | [集成 §3 — 发布版本](docs/integration.zh-CN.md) |
| **想了解原理** | 理解设计 | [架构文档](docs/architecture.zh-CN.md) |

[集成指南](docs/integration.zh-CN.md)覆盖完整链路 —— 配置(`lode.toml`)→ 运行(`state.json`)→ 发布(`manifest.json`)。

完整文档索引(双语):[`docs/`](docs/README.zh-CN.md)。可运行示例:
[`tests/apps`](tests/apps)(一个 Rust + 一个 Bun 服务)与 [`tests/compose`](tests/compose)(实时更新/回滚)。

## 快速开始

让 lode 指向一份已签名的 manifest,再运行通用镜像。默认 lode 读取 `/srv/lode/lode.toml`,
状态也存在 `/srv/lode` 下:

```bash
docker run --rm \
  -v "$PWD/lode.toml:/srv/lode/lode.toml:ro" \
  -e LODE_TRUSTED_KEYS="<key_id>:<base64-公钥>" \
  docker.io/bkhq/lode:latest
```

一份最小的 `lode.toml`(全部选项见 [`docs/lode.example.toml`](docs/lode.example.toml)):

```toml
[global]
app = "myapp"
[update]
manifest = "https://releases.example.com/myapp/manifest.json"   # 或:github = "owner/repo"
asset    = "myapp-linux-x86_64.tar.gz"                          # 本机要安装的资产文件名(精确匹配)
policy   = "auto"                                               # off | check | auto
[command]
run = "./myapp"                                                # 如何启动应用(字面命令,cwd = 版本目录)
[trust]
require_signature = "enforce"
```

> 首次运行若 `/srv/lode/lode.toml` 不存在,lode 会在那里生成一份起始配置并提示你填写来源。
> 用 `LODE_DIR` 改基目录。若改用 `--manifest`/`--github`(或 `LODE_*`)则无需配置文件。

要构建你自己的应用镜像,把 lode 叠到任意基础镜像上:

```dockerfile
FROM oven/bun:1                       # 或任何你的应用所需运行时
COPY --from=docker.io/bkhq/lode:latest /usr/bin/lode /usr/bin/lode
ENTRYPOINT ["/usr/bin/lode"]
```

## 工作原理

```
通用镜像                  ┌─────────────────────────────────────┐
zzci/ubase         ────► │  lode  (静态 Rust 二进制)            │
                         └───────────────────┬─────────────────┘
                                             │ lode.toml + 环境变量 + CLI
                                             ▼
   [update].manifest ──HTTPS(+headers)──► manifest.json  (channels → versions → assets[name])
                                             │  (远程;从不落本地)
                        按文件名选择资产 ────┤── 下载 → 校验 sha256 + 签名
                                             ▼
                    $LODE_DIR/versions/<ver>  ──(原子 rename)──► current
                                             │
                                             ▼
              lode            → 执行 `run`  (受监督服务:自动更新 + 回滚)
              lode <args…>    → 执行 `exec` + <args>  (一次性 CLI 直通)
```

## 一个二进制,两种身份

lode 是 **multi-call 二进制**。以 `lode` 调用是加载器,**没有任何子命令** —— 参数会转发给应用。
注意:lode 先解析自己的旗标(`--version`、`--help` 及 `LODE_*` 全局选项),应用的前导参数若与之
同名会被 lode 吃掉;用 `lode -- <args…>` 可将旗标形式的参数原样直通。以 **`lode-cli`**(随之发布
的软链接)调用则是运维 / 发布工具箱。

| 调用 | 作用 |
|---|---|
| `lode` | 启动并监督应用(`[command].run`);按策略自动更新 |
| `lode <args…>` / `lode -- <args…>` | 直通:执行 `[command].exec` + `<args>`(如 `lode run db:init`);参数与 lode 旗标同名时用 `--` |
| `lode-cli status` / `update` / `rollback` / `restart` / `versions` / `pull` / `seed` | 管理运行中的实例(经 `state.json`);`seed` 离线安装本地版本,用于开发/测试 |
| `lode-cli self-update` | 用最新的 lode release 替换当前这个 lode 二进制(以内置的发布公钥校验);重启 lode 后生效 |
| `lode-cli keygen` / `sign` / `verify` / `manifest` / `manifest-sign` / `delegate` / `revoke` / `publish` / `init` | 发布方 / 运维工具 |

## 三个文件

- **`lode.toml`** —— 本地 TOML;运维的配置(如何拉取与运行)。应用从不写它。→ [`docs/lode.example.toml`](docs/lode.example.toml)
- **`state.json`** —— 本地 JSON;运行时通信。lode 写状态,应用写请求(`target`/`restart_nonce`/`check_nonce`/`ready`)。→ [集成 §2](docs/integration.zh-CN.md)
- **`manifest.json`** —— 远程 JSON;已签名的版本目录(从不落本地)。→ [`docs/manifest.example.json`](docs/manifest.example.json)

## 关键行为

- **更新** `[update].policy = off | check | auto`;来源是 `manifest`(原生 `lode/v1` JSON)**或** `github = "owner/repo"`(Releases)。
- **回滚** —— 新版本若在 `health_grace` 内退出,回滚到上一个已知良好版本(单次触发)。
- **重启** `[supervise].restart = off | on-failure | always` —— `on-failure`(默认,keep-alive)对失败的应用重试 `restart_max` 次后**暂停**(lode 保持存活,容器绝不陷入崩溃循环);`off` 退回镜像子进程的旧行为;lode 主动发起的更新/回滚/重启总会重新拉起。
- **信任** —— `sha256` + `ed25519`(或 ECDSA P-256 / P-384,算法标在密钥上);设 `[trust].trusted_keys` + `require_signature = off | auto | enforce`。注意:校验默认为 `auto`(仅在配置了受信密钥时才强制)—— 生产环境请设 `require_signature = "enforce"`。根密钥可以离线:由它把签名委托给短期子密钥,并可选订阅吊销列表(`[trust].revocation_url`)。签名是发布方的事 —— 见[集成 §3](docs/integration.zh-CN.md)。
- **资源** —— `[[resource]]` 按同一套信任模型分发配置文件、数据等资源包,以 `<app>/<name>` 发布:在 app 启动前安装,以 `LODE_RESOURCE_<NAME>` 交给 app,按各自的 `policy` 更新(通过 `SIGHUP` 或重启通知 app),也可经 `lode-cli pull` 或 SDK 按需拉取。→ [架构 §5](docs/architecture.zh-CN.md)
- **私有源** —— `[http].headers`(支持 `${ENV}` 展开)用于 manifest 和同源资产的鉴权;其他主机只有列入 `[http].credential_hosts` 才会收到。私有 GitHub 仓库用 `[update].github_token`。
- **状态上报** —— `[report]` 通过 OpenTelemetry OTLP/HTTP 推送每台主机的状态(版本、运行状态、重启次数、可用更新)和生命周期事件,可发往任何 OTLP 接收端或 lode-server registry。→ [架构 §8](docs/architecture.zh-CN.md)
- **发布** —— 把签名后的资产挂到 GitHub Releases、自托管原生 manifest,或用 `lode-cli publish` 发布到 [lode-server](https://github.com/bkhq/lode-server) registry(默认 `https://lode.bk.io/u`)。→ [集成 §3](docs/integration.zh-CN.md)

## 作为库使用

lode 由三个 crate 组成:**`lode-core`**(不含 clap、不碰信号——配置、manifest 解析、
校验下载/安装、`Engine` 门面)、**`lode-supervisor`**(监管循环 + 就绪/停止握手,构建在
`lode-core` 之上)、**`lode`**(二进制:clap + 发布工具)。嵌入前两者不会带上 CLI,也不会
自动安装 lode 的进程级副作用——那些通过 `InitOptions` 显式开启。

这些 crate **没有发布到 crates.io**(那里的 `lode` / `lode-core` 属于无关项目)——请依赖本仓库的
发布 tag:

```toml
[dependencies]
lode-core = { git = "https://github.com/bkhq/lode", tag = "v0.1.0" }        # 配置 + Engine(无 clap、无信号)
lode-supervisor = { git = "https://github.com/bkhq/lode", tag = "v0.1.0" }  # + 监管循环,由注入的 SignalSource 驱动
```

完整指南见 [docs/embedding.zh-CN.md](docs/embedding.zh-CN.md):程序更新自己的可执行文件
(`SelfUpdate`),以及宿主监管另一个应用(`Engine` + `serve_embedded`)。两者在
[`examples/embed`](examples/embed) 里都有完整、可运行的示例程序。

```bash
cargo run -p lode-core --example engine        # 代码里构造配置 + 只读 Engine
cargo run -p lode-supervisor --example embedded # 宿主自持信号,无 subreaper/flock
```

## 从源码构建

```bash
cargo build --profile dist --target x86_64-unknown-linux-musl    # 静态发布二进制
cargo fmt --all -- --check && cargo clippy --workspace --all-targets   # 静态检查
cargo nextest run --workspace && cargo test --doc --workspace          # 测试
cargo build --bins && cd tests && bun install && LODE_BIN=../target/debug/lode bun test --timeout 30000 src/   # e2e
```

工具链固定在 `rust-toolchain.toml`;最低支持的 Rust 版本是 1.96。

## 许可证

MIT
