# 运维 lode

[English](operations.md) · **中文**

安装 lode、`lode-cli` 运维命令、日志、退出码和常见问题。配置见
[`lode.example.toml`](lode.example.toml) 和[架构 §10](architecture.zh-CN.md);app 一侧见
[集成](integration.zh-CN.md)。

---

## 1. 安装

### 容器镜像

`docker.io/bkhq/lode` 带有 `/usr/bin/lode` 和 `/usr/bin/lode-cli`,入口是 `lode`。标签为发布 tag
(如 `v0.0.1`)和 `latest`。每个标签另有一个 `-slim` 变体(`v0.0.1-slim`、`latest-slim`),基于
`debian:13-slim` 而非 `zzci/ubase`:带 glibc、shell 和 `apt`,bun/node/deno 运行时照常可用,体积约为
默认镜像的三分之一。它不含 `ca-certificates`;lode 自身不需要,读取系统信任库的应用需自行安装。另有
`-mini` 变体(`v0.0.1-mini`、`latest-mini`),基于 `gcr.io/distroless/static-debian13`,总共几 MB:适用于
应用本身是静态二进制的场景。它带 CA 证书、tzdata 和 `nonroot` 用户,但没有 libc 和 shell,动态链接的
应用或下载的 bun/node/deno 运行时无法在其中启动。要把 lode 加进自己的镜像,从任一镜像中复制二进制即可:

```dockerfile
COPY --from=docker.io/bkhq/lode:latest /usr/bin/lode /usr/bin/lode
RUN ln -s lode /usr/bin/lode-cli
ENTRYPOINT ["/usr/bin/lode"]
```

### 二进制

每个 [release](https://github.com/bkhq/lode/releases) 按平台各有一个压缩包——
`lode-linux-x64`、`lode-linux-arm64`、`lode-darwin-x64`、`lode-darwin-arm64`——旁边各有一个
`.sig`,另有一个 `SHA256SUMS`。压缩包里是 `lode` 和 `lode-cli` 软链接。

```bash
base=https://github.com/bkhq/lode/releases/latest/download
curl -fsSLO "$base/lode-linux-x64.tar.gz" -O "$base/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf lode-linux-x64.tar.gz -C /usr/local/bin lode lode-cli
```

Linux 二进制是静态链接(musl),可在任何发行版上运行。

### Windows

`lode-windows-x64.zip` 里是 `lode.exe` 和 `lode-cli.exe`(同一个程序的两个名字)。把两者解压到
`PATH` 里的某个目录即可。

在 Windows 上 lode 同样可以监督和更新 app。差异如下:

| 方面 | Windows 上的行为 |
|---|---|
| 停止 app | lode 先把 `status` 置为 `stopping` 或 `updating`,再向 app 的进程组发送 **`CTRL_BREAK`**。超过 `stop_timeout` 后终止 app 的整棵进程树。Node 收到的是 `SIGBREAK`,Go 收到的是 `os.Interrupt`;忽略它的程序会在超时后被杀掉。 |
| 进程树 | app 运行在一个 Job Object 里,lode 无论以何种方式退出,这个 Job 都会关闭。app 启动的任何进程都不会比 lode 活得久。 |
| 停止 lode | Ctrl-C、Ctrl-Break、关闭控制台、注销和关机都会先优雅停止 app。 |
| 信号 | 没有可转发的信号:`[signals]` 不起作用。 |
| `lode <args…>` | 没有 `exec`:app 作为子进程运行,lode 以它的退出码退出。 |
| `[runtime]` | 运行时二进制是 `<name>.exe`。 |
| 数据目录 | 默认 `C:\ProgramData\lode`。没有 `current` 链接;当前版本记录在 `state.json` 里。 |
| `lode-cli self-update` | 同时替换 `lode.exe` 和 `lode-cli.exe`。运行中的可执行文件不能被覆盖,所以旧文件先被改名挪开,在 lode 下次启动时删除。 |
| 控制台 | `CTRL_BREAK` 要求 lode 和 app 共用一个控制台。没有控制台时 app 必须自己关注 `state.json` 里的 `status`,否则会在 `stop_timeout` 后被终止。 |

### 升级 lode 自身

```bash
lode-cli self-update                     # 最新发布
lode-cli self-update --version v0.0.1    # 指定 tag;降级也用它
```

`self-update` 下载本机对应的发布制品,按**当前二进制内置的**发布公钥校验,然后就地替换可执行文件。
它不读取 app 的 `lode.toml`。正在运行的监督进程在 lode 重启之前仍使用旧二进制。可执行文件所在目录必须可写。

如果该发布是用已安装二进制不认识的密钥签的(密钥轮换之后),显式传入新公钥:
`--release-key '<key_id>:<base64>'`。

---

## 2. 命令

`lode` 是加载器,没有子命令;`lode-cli` 是工具箱。两者都接受[架构 §10](architecture.zh-CN.md)
的全局选项(`--dir`、`--config` 等)和对应的 `LODE_*` 变量,所以要让 `lode-cli` 指向与运行实例
相同的数据目录。

| 命令 | 作用 |
|---|---|
| `lode` | 启动并监督 app(`[command].run`)。会加单实例锁。 |
| `lode <args…>` | 以 `[command].exec` + `<args>` 替换 lode 进程运行:不加锁、不监督。退出码就是 app 的退出码。参数形似 lode 选项时用 `lode -- <args…>`。 |
| `lode-cli status` | 打印解析后的配置、远程来源摘要和 `state.json`。离线可用。 |
| `lode-cli versions` | 列出已安装版本;`*` 标记当前版本。 |
| `lode-cli update [--version <v>]` | 下载、校验并安装 channel 的最新版本,或指定的 `<v>`。 |
| `lode-cli rollback [--version <v>]` | 请求切换到上一个良好版本,或指定的 `<v>`。 |
| `lode-cli restart` | 请求在当前版本上重启 app。 |
| `lode-cli pull [--resource <name>] [--version <v>]` | 拉取、校验并激活 `[[resource]]` 资源包:全部,或 `<name>` 的渠道最新版或 `<v>`(更旧的 `<v>` 即回滚)。实例运行时由其应用并通知 app。 |
| `lode-cli self-update` | 替换 lode 二进制(见 §1)。 |
| `lode-cli init [path]` | 把起始 `lode.toml` 写到 `path`,不给则打印。 |
| `lode-cli seed <file>` | 把本地文件安装成一个版本,用于开发。见[本地测试](dev-local-testing.zh-CN.md)。 |
| `lode-cli keygen` / `sign` / `verify` / `manifest` / `manifest-sign` / `publish` | 发布者命令。见[集成 §3](integration.zh-CN.md)。 |

### 管理命令如何作用于运行中的实例

`update`、`rollback`、`restart` 不向监督进程发信号。它们把请求写入 `state.json`,监督进程大约
每秒轮询一次:

- **`update`** 先安装版本。有监督进程在跑时再写 `target`,由监督进程热更新;没有时直接激活该版本,
  下次 `lode` 启动就运行它。
- **`rollback`** 写 `target`。它是纯本地操作,不下载任何东西,所以目标版本必须已经安装。
- **`restart`** 递增 `restart_nonce`。监督进程在重新拉起前会重读 `lode.toml`,所以修改配置后也是
  用它生效。

```console
$ lode-cli versions
lode versions (2 installed)
* 1.1.0
  1.0.0
$ lode-cli rollback --version 1.0.0
lode rollback: target set to 1.0.0
$ lode-cli restart
lode restart: requested (restart_nonce=1)
```

`status` 的末尾是 `state.json` 的内容。其中 `status` 取值为 `starting`、`running`、`held`、
`updating`、`rolling-back`、`stopping`、`stopped`、`error` 之一:

```console
$ lode-cli status
…
[state] /srv/lode/state.json
{
  "current": "1.0.0",
  "last_good": "1.0.0",
  "status": "running",
  "pid": 2056087,
  "config_generation": 0,
  "checked_nonce": 0,
  "restart_nonce": 0,
  "check_nonce": 0,
  "hold": false
}
```

---

## 3. 日志

lode 把日志写到 **stdout**,每个事件一行,与 app 的输出在一起(app 继承 lode 的 stdout 和
stderr):

```
2026-10-10T13:13:12.412945Z  INFO spawned child version="1.0.0" pid=2056087 instance="2056085-k3x9q2w7m4p8zt6n"
2026-10-10T13:13:14.414835Z  INFO restart requested via state.json nonce=1
2026-10-10T13:13:16.468901Z  INFO termination signal received; stopping child
```

- 级别:`[global].log_level`、`--log-level` 或 `LODE_LOG_LEVEL`——`trace`、`debug`、
  `info`(默认)、`warn`、`error`。
- 日志行带 ANSI 颜色;设 `NO_COLOR=1` 输出纯文本。
- 命令失败时打印 `ERROR lode failed: <原因>` 并以非零退出。`lode-cli publish` 例外:它在 stderr
  上报告 `publish: <原因>`。
- 请求头的值、令牌和私钥从不写入日志;URL 写入日志时不带查询串。

### 状态上报

要集中查看一批主机,设置 `[report]`:lode 会通过 OpenTelemetry OTLP/HTTP 推送每台主机的状态
(应用版本、运行状态、重启次数、可用更新)和生命周期事件(启动、停止、更新、回滚、暂停):

```toml
[report]
endpoint = "https://lode.bk.io/r/otlp"   # 任何 OTLP/HTTP 接收端
headers  = ["Authorization: Bearer ${LODE_REGISTRY_TOKEN}"]
# interval = 300
```

- lode-server registry 在包的"观测"标签页展示这些主机;令牌需要对该包有授权。
  OpenTelemetry Collector 或其他 OTLP 后端使用同样的配置块,换成各自的 endpoint 和请求头即可。
- 也可以用 `--report-endpoint` / `LODE_REPORT_ENDPOINT` 和 `--report-interval` /
  `LODE_REPORT_INTERVAL` 设置,无需修改 `lode.toml`。
- 接收端出错只会记录 `status reporting: …` 警告,不影响监管。`export partly rejected`
  表示接收端拒收了数据——对 lode-server 来说,通常是令牌对该包没有授权,或 `[global].app`
  不是包标识。
- 修改 `[report]` 需要重启 lode。

---

## 4. 退出码

| 情形 | 退出码 |
|---|---|
| `lode-cli` 命令成功 | `0` |
| 命令失败(配置错误、锁被占用、拉取或校验出错等) | `1` |
| `lode <args…>` 透传 | app 自己的退出码 |
| 被监督的 app 退出且 lode 随之退出(`restart = off`,或 `on-failure` 下的正常退出) | app 的退出码;被信号杀死时为 `128 + 信号号` |
| lode 收到 `SIGTERM` / `SIGINT` / `SIGQUIT` | 优雅停止后 app 的退出码 |

在 `restart = on-failure`(默认)和 `always` 下,app 持续失败时 lode **不会**退出:它以
`status = error` 暂停并保持存活。

---

## 5. 常见问题

**`lock: another lode instance is already running (pid …)`**
另一个监督进程占用了这个数据目录。换一个 `--dir`,或停掉那个实例。被杀死的 lode 留下的锁会自动回收。

**app 处于暂停状态(`status = error`)**
app 连续失败了 `restart_max` 次,或回滚目标也失败了。lode 打印
`app failed to stay up — pausing` 后等待。修好原因后,任选其一:编辑 `lode.toml`(lode 会重读并
重试)、执行 `lode-cli restart`,或用 `lode-cli update --version <v>` 装另一个版本。

**`no trusted keys configured; asset is UNVERIFIED`**
`require_signature = auto` 且没有配置公钥:此时只校验 sha256。请设置 `[trust].trusted_keys`,
生产环境用 `require_signature = "enforce"`。

**`delegation rejected: delegation expired (not_after=…, now=…)`**
manifest 由委托子密钥签名,但其 delegation 已过期,或本机时钟不对。由发布方续签 delegation(`lode-cli delegate`)并重新签名;否则检查时钟。`lode-cli status` 会显示签名密钥及其 delegation 的到期时间。`delegated key … is revoked` 表示该子密钥在 `[trust].revocation_url` 列表中(已合并进 `$LODE_DIR/revocations.json`)。

**`manifest: no update source configured`**
`[update].manifest` 和 `[update].github` 都没设。对 seed 出来的离线数据目录这是正常的;`update`
需要来源。

**`refusing non-HTTPS URL`**
远程拉取必须走 HTTPS。明文 `http` 只对回环地址放行,除非设置 `[http].allow_insecure = true`。

**`header references unset env var ${NAME}`**
`[http].headers` 里的 `${NAME}` 在每次请求时展开,必须存在于 lode 的环境变量中。

**新版本被回滚了**
新版本在 `health_grace` 内退出,或没有在 `ready_timeout` 内报告就绪。`state.json` 的 `history`
里会记下 `result: "bad"`,原因在 `last_error`。

**改了 `lode.toml` 但没有变化**
编辑配置不会重启正在运行的 app。lode 会递增 `config_generation`;用 `lode-cli restart` 让改动生效。

**app 的参数被 lode 吃掉了**
lode 先解析自己的选项。用 `lode -- <args…>` 把所有参数原样透传。
