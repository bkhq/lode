# lode 架构文档

[English](architecture.md) · **中文**

> 通用的"升级 + 启动"组件:一个**小型 Rust 静态二进制**(几 MB),语言无关。
> 校验**文件完整性(sha256)**与**发布者身份(ed25519 / ECDSA 签名)**,再启动并管理服务,支持无缝热更新与回滚。
> 通用镜像 = `zzci/ubase` + lode 二进制(无需任何语言运行时;运行时若有则启动时下载并缓存);一个实例 = **单程序、单通道**。

本文描述 lode 的工作方式:模型、行为、文件格式与配置。

---

## 1. 目标与定位

- **载入器 (lode)**:固定不变的"升级+启动"组件,编译为一个小静态二进制,打进通用镜像一次,永不重建。职责:配置 → PID 锁 → 决定该跑哪个版本 → 必要时下载/**校验(完整性+身份)**/安装 → 原子启用 → 以**子进程**启动并**管理服务** → 按策略热更新/回滚 → 经 **`state.json`** 与 app 沟通。
- **应用 (app)**:任意语言的程序,打成包并由可信发布者签名,发布到 manifest;lode 校验通过后运行。**lode 不绑定应用语言**(运行方式由 `lode.toml` 的 `[command]` 指定)。
- **体积**:lode 是几 MB 的 musl 静态二进制,可在**任意**基础镜像上运行(scratch、distroless,或 `zzci/ubase` 这类更完整的镜像)。
- **关系**:`通用镜像(zzci/ubase + lode)` + `远程 manifest/签名包` → 容器启动即按策略校验并运行应用。换应用 = 换 `[update].manifest`(env `LODE_MANIFEST`),镜像零重建。

### 设计原则

1. **单程序、跟随单通道**:一个 lode 只管一个 app;manifest 可定义**多个通道**(`[channels.*]`),lode 实例按 `LODE_CHANNEL` **跟随其一**;版本暴露**具名资产(assets)**,operator 按文件名(`[update].asset`)选其一 —— 无平台探测。
2. **三文件分工**:`lode.toml`(本地 TOML,纯配置,**app 不写**)、`state.json`(本地 JSON,**lode 与 app 都写**,通信中枢)、`manifest.json`(**远程 JSON**,不落本地)。app 通过**读写 `state.json`**(读 `available`、写 `target`/`restart_nonce`)与 lode 沟通;其它实时/RPC 通信属带外,由 app 自理。详见 §7。
3. **下载靠 manifest,运行靠 lode.toml**:artifact 的 `format` 决定如何落地;运行命令 `run`/`exec`(+ 可选 `[runtime]`)写在 lode.toml,**无 `kind`**。
4. **不主动跳版本**:仅当本地无任何已装版本时才下载最新;有版本先起现有版本,更新交策略与 app/CLI 触发。
5. **双层校验**:完整性(sha256)+ 发布者身份(默认 ed25519,或 ECDSA P-256/P-384),按 `trust.require_signature` 强制。
6. **子进程启动 + 服务管理**:lode 永远以子进程拉起并监督,提供起停/重启/状态;就绪/停止握手避免提前杀进程。
7. **统一配置**:`CLI > 环境变量(`LODE_*`) > `lode.toml`(TOML) > 默认值`。
8. **私有源**:`[http].headers` 透传鉴权;私钥/凭据不进镜像。
9. **一个二进制,三个 crate**:`lode-core` 与 `lode-supervisor` 是可嵌入的库,`lode` 是基于它们的二进制(见 §14)。

---

## 2. 总体架构

```
通用镜像(构建一次)         ┌──────────────────────────────────┐
zzci/ubase                 │  lode (Rust 静态二进制, 几 MB)   │
                          └────────────────┬─────────────────┘
                  lode.toml (本地配置) │ 读 CLI / env / lode.toml
                                           ▼
   update.manifest ──HTTPS(+headers)──► 远程 manifest.json (channels + versions→assets[name])
                                           │  (远程,不落本地)
                       ┌───────────────────┼────────────────────────────┐
                       ▼                   ▼                            ▼
              按文件名(asset)选资产  下载 → 校验 sha256 + ed25519      受信公钥
                       │                            │ 全过 → 按 format 落地/解包
                       ▼                            ▼
            current ─► versions/<ver>  (rename 原子切换;每版本存元数据供离线)
                       │
                       ▼
            lode 以子进程启动并管理:  <lode.toml [command].run/exec>  (起停/重启/热更/回滚)
                       │                        ▲
            $LODE_DIR/state.json (lode+app 共写) │ 轮询 state.json mtime
                       └────────► app ◄───────────┘ app 读 state.available 提示 / 写 state.target|restart_nonce 触发
```

---

## 3. 运行形态:一个小型静态二进制

- lode 是**单一静态二进制**:Linux(x86_64 / aarch64)为 `*-unknown-linux-musl` + `+crt-static`,另有 macOS 构建(x86_64 / arm64)。另有 Windows 构建(x86_64);它的差异见[运维](operations.zh-CN.md#windows)。
- **镜像**:`FROM zzci/ubase`(通用基础镜像,带 libc/shell/工具,也能承载 lode 启动时下载的运行时)→ `COPY lode /usr/bin/lode` → `ENTRYPOINT ["/usr/bin/lode"]`。lode 的 TLS 根证书是内置的(webpki-roots),无需系统 CA 证书。lode 是静态二进制,在极小基础镜像(`scratch`/distroless)上同样能跑——当 app 自带运行时或本身就是静态二进制时。
- **TLS**:rustls + aws-lc-rs provider;不使用 OpenSSL。HTTP 为同步实现——lode 不带异步运行时。
- **不写 `unsafe`**:每个 crate 都禁止 unsafe 代码。子进程以 argv 数组拉起,不经 shell。

---

## 4. 语言无关的应用模型:format(打包) + run/exec(运行)

职责分工:**落地交给 manifest 的 `format`,运行交给 `lode.toml` 的 `[command]`**。lode 自身只做选取/下载/校验/启用/监督。

### format —— 打包格式(如何把下载物落地)

| format | 含义 | 落地动作 |
|---|---|---|
| `raw` | URL 即最终单文件(二进制或脚本) | 下载 → 存为 `versions/<v>/<资产文件名>` |
| `gz` | 单文件 gzip(`.gz`) | 下载 → gunzip → 单文件 |
| `tar.gz` | 目录树的 gzip tar(`.tar.gz`/`.tgz`) | 下载 → 解包到 `versions/<v>/` |
| `zip` | zip 包 | 下载 → 解压到 `versions/<v>/` |

**`format` 始终由资产文件名的扩展名推导**(最长匹配 —— `.tar.gz`/`.tgz`→`tar.gz`、`.gz`→`gz`、`.zip`→`zip`,否则 `raw`;见 §12),不入 manifest、不参与签名。**sha256 始终对"下载到的原始文件(解包前)"计算。**

### 如何运行(`[command]` 段的 `run` / `exec`,无 `kind`)

"如何运行"由 operator 在 `lode.toml` 的 `[command]` 段配置;manifest 只声明"下载什么"(`name`/`url`/`sha256`)。`run` 与 `exec` 均为**字面命令**(按空白切成 argv;cwd=`workdir`,默认:版本目录),唯一的占位符是 **`{dir}`**=版本目录。**argv 直接执行,不经 shell;不需要 `kind`。**

- **`run`**:**裸跑 `lode` 时启动 app 的命令**。例 `"bun run"` 或 `"./myapp serve"`。
- **`exec`**:**`lode <args>` 透传时的基准命令**,CLI 参数追加其后。例 `"bun"` → `lode run db:init` 即 `bun run db:init`。
- **`workdir`**:子进程 cwd,`{dir}`(默认)或绝对路径。stdio 继承 lode。
- **Manifest 资产覆盖**:manifest 资产可携带可选的 `run`/`exec` 字段,当存在时**覆盖** operator 的 `[command].run`/`exec`。这些字段被绑入 per-artifact 签名消息和目录签名——在 `require_signature=auto`(有密钥)或 `enforce` 下,被篡改的目录无法注入恶意启动命令。
- **自动 chmod+x**:解压后,lode 对有效 `run`(以及不同时的 `exec`)的第一个空白 token 做 chmod+x,条件是该 token 是相对路径且解析到版本目录内的文件。
- **缺少 run → 硬报错**:若 `[command]` 与 manifest 资产均未提供 `run` 命令,启动失败,提示 `no run command: set [command].run or publish \`run\` in the manifest asset`。

### 运行时下载(`[runtime]`,可选)

当 `run`/`exec` 依赖某运行时(如 `bun`)时,可在 `lode.toml` 的 `[runtime]` 声明。解析顺序 **PATH → 缓存 → 下载**:先查 PATH;否则复用 `$LODE_DIR/runtime/<key>/<name>` 里上次下载的运行时;再否则从 `download` 下载。自包含二进制则省略此表。

- **format / 摊平**:`format` 由 URL 扩展名推导(`raw`/`gz`/`zip`/`tar.gz`);解包后把目标二进制**摊平到 `runtime/<key>/<name>`**,所以官方嵌套包(bun 的 `bun-linux-x64/bun`、node 的 `node-vX/bin/node`)和扁平包(deno)、单文件都能用。
- **缓存(按运行时配置分键)**:缓存目录是 `runtime/<key>/`,`<key>` **始终**包含 `download` URL 的 12 位十六进制摘要,锁定且版本串是合法路径分量时再加版本前缀——`1.1.38-9f2c0a4b6d18`,未锁定则是 `url-9f2c0a4b6d18`。摘要保证键的精确性(`version` 不变、只把 `download` 指向别的字节也会失效),前缀保证目录可读。落地的 `runtime/<key>/<name>` 在后续启动被复用——`$LODE_DIR` 持久化时下载只发生一次——但**仅限同一份 `[runtime]` 配置**:改了 `version`/`download` 就换了 key,新运行时会下载到自己的目录,既不会复用旧缓存,也不会解压到旧目录上。当前 key 确定后,`runtime/` 下其余条目会被回收;删掉 `runtime/<key>/` 即可强制重下。
- **不校验**:与应用产物不同,运行时下载**不带 `sha256`/`sig`,不做完整性/身份校验**。请锁版本、用可信源,若需凭据把其 host 加入 `[http].credential_hosts`。
- **版本锁定**(`version` + `version_check`):设了 `version` 后,lode 对即将使用的运行时(PATH/缓存/刚下载)跑 `version_check`(默认 `--version`),要求输出**包含** `version`。PATH/缓存里版本不对会被绕过去重新下载;下载下来还不对则硬报错。
- 最后把 `$LODE_DIR/runtime/` **前置到子进程 PATH**。

```toml
[runtime]
runtime       = "bun"                                    # run/exec 用到的可执行名
download      = "https://example.com/bun-linux-x64.zip"  # PATH/缓存 都没有 bun 时下载
version       = "1.3.14"                                 # 可选:要求此版本(探测输出的子串)
# version_check = "--version"                            # 可选:打印版本的参数(默认 --version)
```

### 资产选择(按文件名)

版本的 `assets[]` 以**文件名**(`name`,如 `myapp-linux-x86_64.tar.gz`)为键。operator 用 `[update].asset` 指定本机要装的资产;lode 按 `name` 在来源的资产列表里匹配。**无平台自动探测、无 arch 别名表** —— 文件名是唯一选择键(native 与 GitHub 两源同一键),其扩展名确定 `format`。文件名按约定带上品牌/平台。见 §6/§12 与 [`docs/source-adapters.zh-CN.md`](source-adapters.zh-CN.md)。

---

## 5. 版本生命周期与更新策略

### 启动决策(不主动跳版本)

```
读 state.json 得 current
┌─ current 不存在(首次/全新数据盘)
│     → 引导:拉远程 manifest → 取 latest → 选 artifact → 下载 + 校验(sha256+sig)+ 落地
│     → current = latest, last_good = latest → 起子进程
└─ current 已存在
      → 直接启用并起 current(不等网络, 快速启动)
      → 之后按策略在后台处理更新
```

current 不存在且既无 `[update].manifest` 也无本地可用版本 → 报错退出。离线/气隙:lode 从不读取本地 manifest —— 用 `lode-cli seed` 在本地安装版本(或连同 `.lode.json` 标记一起放好 `versions/<ver>/`),lode 即可无网启动 `current`;见 `docs/dev-local-testing.md`。

### 更新策略 `update.policy` = `off` | `check` | `auto`

| 取值 | 中文 | 行为 |
|---|---|---|
| `off` | 关闭 | 不做后台检查;只跑 current/pinned。仍尊重显式 `target`(按需拉一次远程解析条目)。 |
| `check`(默认) | 检测 | 周期拉远程 → 刷新本地 catalog、写 `state.available`,**不自动应用**;app 读到后提示 → 写 `state.target` → lode 执行。 |
| `auto` | 自动 | 周期拉远程 → `latest > current` 则自动设 `target=latest` 并热更。 |

`update.pin` 锁版本(等价 `off` + 固定 target);任何策略下显式 `state.target` 都被尊重并执行(前提校验通过)。检查节奏 `update.check_interval` 秒(0=仅启动查一次;`off` 不计时)。

### 应用一个 target(热更新 + 回滚)

```
target ≠ current 且可用(stop-start 模式):
  1. 确保已安装(缺则下载 + 校验 + 落地到 versions/<target>)
  2. (readiness=state 且在跑的 app 已用 "<LODE_INSTANCE>-0" 报就绪以选用本机制)app 自定节奏的切换——只暂存,先不切:写 state.ready="<LODE_INSTANCE>-1" 提示在跑的 app,置 status=updating,然后等待(默认 app 自定节奏,`prepare_timeout`=0;非 0 的 `[supervise].prepare_timeout` 给等待设上限);app 自行做准备(排空/落盘/预热),好了写 state.ready="<LODE_INSTANCE>-2" 应答。其间旧子进程继续服务且受监管(此时崩溃就直接应用已暂存的 target)。readiness=none、或使用裸 token 的 app,跳过本步、立即切换。
  3. 写 state.status=updating → 给旧子进程 SIGTERM(app 清理后退出)→ STOP_TIMEOUT 超时才 SIGKILL
  4. 原子切 current 软链 → versions/<target>
  5. 起新子进程;等就绪(§8:readiness=none→存活满 grace;readiness=state→等 state.ready="<新 LODE_INSTANCE>-0")
  6. 就绪 → status=running、last_good=target;**单次失败即回滚**:就绪超时 / 观察期(health_grace)内退出或崩溃一次 → 切回 last_good 并同样观察之;若 last_good 在其 grace 内也失败 → lode **暂停**(status=error,keep-alive,§8)而非退出;`restart = off` 时则镜像退出
```

### 启动清理(GC + 孤儿子进程)

`lode` 启动(服务模式)在确定版本之前先做一遍清理:

- **孤儿子进程**:上一个 lode 崩溃可能遗留仍在跑的 app 子进程。接管僵尸锁后,读 `state.json.pid`,若该进程还活着 → 优雅终止(SIGTERM→超时 SIGKILL)再起新的,避免端口/资源冲突与双实例。
- **垃圾回收**:清理中断遗留的 `downloads/<v>/*.part`、`versions/<v>.tmp` 半成品(已校验的按版本下载缓存保留);按 `keep_versions` 保留 current + last_good + 最近 N 个版本,其余版本目录**连同其 `downloads/<v>/` 缓存**一起删除;`$LODE_DIR/runtime/` 同理只留当前在用的缓存 key。
- **校验落盘一致性**:`current` 软链指向的版本若缺失/损坏 → 回退到 last_good 或重新引导。

---

## 6. 验证与信任:文件完整性 + 发布者身份

- **完整性**:对下载到的原始文件求 sha256,必须等于 artifact 的 `sha256`(小写 hex)。
- **身份**:发布者签名 —— 默认 ed25519,或 ECDSA(P-256 / P-384);算法随密钥走(见下文*受信公钥*)。发布者私钥签"发布记录",lode 用预置**受信公钥**验签 → 即使 CDN/镜像被投毒或传输被 MITM,也能确认是该发布者所发且未篡改。

### 签名规范字节(精确,UTF-8,`\n` 分隔,无尾换行)

asset 级 `sig` 的签名消息:
```
lode.artifact.v2
{app}
{name}
{version}
{sha256}
{run}
{exec}
```
`{app}` 是**应用名**(manifest 的 `name`,即加载器的 `[global].app`),`{name}` 是**资产文件名**(选择键),`{version}` 是发布版本,`{sha256}` 是原始下载文件的小写 hex 摘要,`{run}` 与 `{exec}` 是 manifest 发布的启动命令覆盖(缺省为空字符串)。签名绑定*哪个应用*、*哪个资产*、*哪个版本*、*哪些字节*、*哪些启动命令*;`format`/`url` 由文件名推导或属 operator 本地,**不**参与签名,故被篡改的 catalog 无法把真签名挪到别的字节、别的资产、别的版本或注入恶意 run/exec 命令上。用 `key_id`(asset.key_id ?? manifest.key_id)对应的受信公钥验签。

> **签的是 sha256(摘要),不是直接对 bin 流签名。** 发布者密钥对上面这串规范文本(其中含 `{sha256}`)签名——等价于"签摘要 + 身份字段"。完整 verify 链:**下载字节 → 重算 sha256 →(完整性)== artifact.sha256 →(身份)验这串签名**。sha256 已把内容绑定,故无需把整个二进制流过签名器;这也是发布签名(如签 `SHA256SUMS`)的常规做法。

可选 manifest 顶层 `sig`,由 `lode-cli manifest-sign` 写入,并在解析/下载任何版本**之前**验证,防增删版本、改 channel `latest`、改 URL:
```
lode.manifest.v1
{name}
{key_id}
{canonical}     # 对 channels + versions 的确定性、去 sig 序列化
```
`{canonical}` 由解析后的 manifest(已排序的 `channels`/`versions`)生成,故签名方与验证方产出完全相同的字节,与 JSON 键序/空白无关;顶层 `sig` 本身不计入被签字节。验证时按 manifest 的 `key_id` 选键(选不中则回退逐一尝试所有受信公钥)。

可选顶层 `expires`(Unix 秒)及其独立签名 `expires_sig`,由 `lode-cli manifest-sign --expires-in <时长>` 写入。到达或超过 `expires` 的 manifest 在任何策略下都会被 lode 拒绝,因此持续返回旧副本的源无法把主机无限期按在旧的 `latest` 上;发布方需在到期前重新签名。`expires_sig` 的被签消息:
```
lode.manifest-expiry.v1
{name}
{key_id}
{expires}
{canonical}     # 与上面相同的序列化
```
它是第二个签名,不属于 `lode.manifest.v1` 消息:早于 `expires` 的 lode 会忽略这两个字段,照常验证未变的 `sig`。这样的 lode 没有过期保护。

`[trust].require_manifest` 决定 manifest 自身必须证明什么:

| 值 | 行为 |
|---|---|
| `off`(默认) | `sig` 与 `expires_sig` 存在则验证,都不强制。 |
| `signed` | 必须带有效的 `sig`。 |
| `fresh` | 必须带有效的 `sig`,以及已签名且未过期的 `expires`。 |

只有 `fresh` 能阻止源去掉 `expires`/`expires_sig` 后重放一份 `sig` 仍然有效的目录。`signed` 与 `fresh` 需要受信公钥和原生 manifest;GitHub 源没有可签名的 manifest,与它们同用会在配置阶段被拒绝。运行中的 lode 把被拒绝的 manifest 当作一次失败的更新检查:继续运行当前版本。

### 受信公钥 / 强度

- `LODE_TRUSTED_KEYS` = 逗号分隔 `[<alg>:]<key_id>:<base64 公钥>`,其中 `alg` 为 `ed25519`(原始 32 字节公钥;省略时的默认)、`ecdsa-p256` 或 `ecdsa-p384`(SEC1 点,规范形式为压缩);或 `LODE_TRUSTED_KEYS_FILE`(每行 `[<alg>:]<key_id> <base64>`)。支持多把(轮换),算法可混用。`key_id` = `sha256(公钥字节)` 的前 16 位十六进制。**算法钉在受信密钥上**:签名用该密钥的算法验证,绝不用 manifest 声明的算法(其 `alg` 仅为提示)。
- base64 值(公钥、种子、签名)统一输出为无 padding 的 **base64url**(RFC 4648 §5,字母表 `A-Z a-z 0-9 - _`);输入同时接受标准 base64,有无 padding 均可。
- `LODE_REQUIRE_SIGNATURE` = `off`(仅 sha256) | `auto`(默认) | `enforce`(生产推荐)。**它只门控 per-artifact 签名。** 目录(manifest 级)签名是 *present 才验* —— 目录携带时才验,永不强制,因此与此设置无关(见下方*目录签名*)。
  - **`auto` 一旦配置了任一受信公钥即变为 fail-closed**:此时 *artifact* 签名成为必需 —— 缺签名*或*验签失败都拒绝。仅当**未**配置任何受信公钥时,`auto` 才跳过 artifact 验签,并把来源记为 **UNVERIFIED**。
  - **`enforce`** 始终要求受信公钥,且 *artifact* 签名必须有效。
  - **目录签名(present 才验)。** 清单可选的顶层 `sig` 在存在时被验证(present 但无效则拒绝),但**永不强制** —— 未签名的目录在 `enforce` 下也能正常安装。channel `latest` 指针的回滚改由客户端**禁降级 floor** 拦截:跟随 `latest` 永远不会解析出比 `max(current, last_good)` 更老的版本;显式 `--version`/`pin`/`rollback` 属刻意选择,豁免。

### 委托签名密钥

受信密钥可以一直离线。受信(根)密钥签发一份 **delegation**,允许一把子密钥在限定时间内为某个 manifest `name` 签名;之后由子密钥签目录(`sig`、`expires_sig`)和资产。运维方仍只信任根密钥。

```json
"delegation": {
  "key": "ed25519:<sub_key_id>:<base64url>",
  "name": "myapp",
  "not_before": 1760000000,
  "not_after": 1762592000,
  "issuer": "<root_key_id>",
  "sig": "<根密钥签名>"
}
```

根密钥签名的内容:
```
lode.delegation.v1
{name}
{alg}            # 子密钥的算法,由根密钥钉住
{sub_key_base64url}
{not_before}
{not_after}
```

- **载体**:原生来源放在 manifest 顶层 `delegation`;GitHub 来源放在名为 `lode-delegation.json` 的 release 资产里(内容相同,需要签名时才拉取)。
- **识别**:带 `delegation` 的 manifest 走委托密钥;不带的即由受信密钥直接签名,行为与以前完全一致。schema 仍为 `lode/v1`。`lode-cli status` 显示 `signing: direct (<key_id>)` 或 `signing: delegated (key <id>, issuer <id>, expires in …)`。
- **有效条件**:delegation 必须由受信密钥签名、`name` 等于本 manifest(即 `[global].app`)、按本机时钟满足 `not_before <= now < not_after`,且子密钥未被吊销。满足后子密钥对该 manifest 及其资产视同受信密钥。只有一层:子密钥不能再委托。
- **策略不变**:`require_signature` / `require_manifest` 以及"未配置受信密钥"的判断只看受信密钥;delegation 永远不能替代它们。
- **过期只影响新安装**:已安装版本启动时不再重新验签。
- **旧版 lode**(≤ 0.0.3)会忽略 `delegation`;配置了受信密钥时会拒绝子密钥签名(fail closed)。用子密钥发布前先升级各主机。

### 吊销列表(可选)

`[trust].revocation_url`(`--revocation-url`、`LODE_REVOCATION_URL`)订阅一份已签名的子密钥吊销列表:

```json
{
  "schema": "lode-revocations/v1",
  "issued_at": 1760500000,
  "revoked": ["<sub_key_id>"],
  "key_id": "<签名密钥 id>",
  "delegation": { "...": "仅在由子密钥签名时携带" },
  "sig": "<签名>"
}
```

签名内容:`lode.revocations.v1\n{key_id}\n{issued_at}`,其后每个被吊销的 id(排序、去重)各占一行。

- 由受信密钥签名,或由持有对 `[global].app` 有效且未被吊销的 delegation 的子密钥签名。
- 每次验证 manifest 时拉取。更新的有效列表(`issued_at` 大于缓存)会**合并**进 `$LODE_DIR/revocations.json`;条目只增不减,列表无法取消吊销,旧列表被忽略。
- 非强制:列表不可达、无效或过旧时只记日志,继续使用缓存。没有列表时,吊销只能依赖 delegation 的 `not_after`;delegation 有效期宜短。

### CLI(发布者)

`lode-cli keygen` / `lode-cli sign <asset> --app <app> --version <ver> --key <priv> [--run <cmd>] [--exec <cmd>]`(或 `--key-env <VAR>`;打印 `sha256`/`sig`/`key_id`,其中 `sig` 即随资产发布的 `<asset>.sig` 内容 —— 或 GitHub 资产 `label`;`--run`/`--exec` 把启动命令覆盖绑入签名)/ `lode-cli verify <asset> --version <ver> --pubkey <b64> --sig <b64>` / `lode-cli manifest <asset> --version <ver> --url <url> [--run <cmd>] [--exec <cmd>] --key <priv> --into manifest.json` / `lode-cli manifest-sign --into manifest.json --key <priv> [--delegation <file>]`(为目录写入顶层 `key_id` + `sig`)/ `lode-cli delegate --app <app> --key <root> --sub <sub.pub> --valid-for 30d --out delegation.json` / `lode-cli revoke --key <priv> [--delegation <file>] --list revocations.json --add <key_id>`。`manifest`、`manifest-sign`、`verify` 支持 `--delegation`,用于以子密钥签名或校验。根私钥离线,lode 只持公钥。

---

## 7. 文件模型与 lode ↔ app 通信

三个概念,职责分明:

| 文件 | 位置 | 格式 | 谁写 | 作用 |
|---|---|---|---|---|
| **`lode.toml`** | 本地 | TOML | **仅 lode 侧**(operator 编写,app 不写) | **app 的配置**:如何启动和配置程序(manifest 地址/通道/exec/workdir/headers/策略/信任/`pin`) |
| **`state.json`** | 本地 | JSON | **lode 与 app 都可写** | 运行时通信中枢:lode 写实际状态,app 写升级/重启请求 |
| **`manifest.json`** | **远程** | JSON | 发布方 | 远程程序的版本目录;lode 按策略拉取,**不落本地**(每版本元数据存于版本目录供离线运行) |

通信全走 **`state.json`**(双向,各自单写字段 + 原子写 temp+rename;lode 轮询其 mtime):

- **lode 写**:`current`/`last_good`/`available`/`status`/`pid`/`last_check`/`last_error`/`history`/`channel`/`config_generation`/`checked_nonce`/`check_error`。
  - **`last_error` 只报告持续性失败**:网络层失败(连接/TLS/重定向/状态码,或响应体没收全)下次检查就会重试,因此要**连续失败 3 次**才写入,且下一次拉取成功即清除——单次失败只记日志,不同步给 app。不会自愈的失败(manifest 格式错、签名不对、磁盘满、新版本起不来)第一次就上报。
- **app 写**(请求):`target`(想升/降到的版本,或 `"latest"`)、`restart_nonce`(递增=请求重启)、`check_nonce`(递增=立即检查更新;lode 把 `checked_nonce` 升到该值作为应答,检查失败则写 `check_error`,否则更新 `available` —— `off` 或有 `pin` 时也一样,只通告不应用)。
- **app 拥有** `hold`:设为 `true` 请求 lode **不要**(重新)启动进程 —— 用于必须在 app 起来之前完成的计划维护。lode 报告 `status = held` 并等待(开机时、子进程退出后、以及 `restart_nonce`/`target` 请求期间都不启动),直到它被清除。它只挡"启动";正在运行的子进程绝不会因此被杀。
- **双方共写**(`readiness=state` 握手,§8):`ready` = `{LODE_INSTANCE}-{相位}`。app 用 `-0` 表示"我能服务了";暂存升级时 lode 用 `-1` 提示在跑的 app;app 用 `-2` 应答"已准备好,可以切了"。lode 读 `-0`/`-2`、写 `-1` 提示,并在切换时清空该字段。app 也可以写裸 `{LODE_INSTANCE}`——这等于不使用准备窗口、直接切换。
- 典型流程(`policy=check`):lode 把发现的新版写进 `available` → **app 读到后,自己决定升级就把 `target` 写成该版本(或 `"latest"`)** → lode 应用并热更。

**通知机制(双向,都不用额外信号):**

- **app → lode**(请求重启/升级):app **原子写 `state.json`**(改 `target` 或递增 `restart_nonce`);**lode 短间隔(~1s)轮询 `state.json` 的 mtime**,变化即重读并执行。**文件本身就是通知**,无需给 lode 发信号(也避免与转发给子进程的信号冲突)。
- **lode → app**(要求重启,让 app 清理):lode 先把 `state.status` 置为 `updating`(热更)或 `stopping`(关停)让 app 能区分,再给子进程发 **`SIGTERM`**;app 在 SIGTERM 处理器里做清理(排空/flush/释放)后 `exit(0)`,须在 `supervise.stop_timeout` 内,否则 `SIGKILL`。退出后 lode 切版本起新进程(§5)。
- **lode → app**(配置变了,**不自动重启**):运行中编辑 `lode.toml` 时,lode **不**重启它(运行中的 app 不被打扰)——而是递增 **`config_generation`**(单调计数器)告知 app 需要重启才能生效。app 自定时机递增 `restart_nonce` 来应用,那次重启会让 lode **重读 `lode.toml`**(新 `[env]`/配置生效)。*暂停*中的 app 则在编辑时自动 reload;宿主进程 env(`-e`/k8s)需重启 lode 自身。

> `lode.toml` 是纯配置,**app 不写它**(只有 lode 侧/operator 写);要锁版本用其中的 `pin`。完整 `lode.toml` 见 `docs/lode.example.toml`。

### `$LODE_DIR/state.json`(lode+app 共写)

```json
{
  "current": "1.4.2",
  "last_good": "1.4.2",
  "available": "1.5.0",
  "channel": "stable",
  "status": "running",
  "pid": 12345,
  "last_check": "2026-06-04T22:00:00Z",
  "last_error": null,
  "history": [ { "version": "1.4.2", "at": "2026-06-04T21:00:00Z", "result": "good" } ],
  "config_generation": 0,
  "checked_nonce": 0,

  "target": null,
  "restart_nonce": 0,
  "check_nonce": 0,
  "hold": false,

  "ready": "12345-k3x9q2-0"
}
```
lode 拥有的字段 vs app 拥有的字段(`target`/`restart_nonce`/`check_nonce`/`hold`)互不重叠;`ready` 为共有字段(§8);`status` ∈ `starting|running|held|updating|rolling-back|stopping|stopped|error`;`result` ∈ `good|bad`。

---

## 8. 子进程监督与服务管理

- 全生命周期由 lode 管理:启动 / 优雅停 / 重启 / 状态 / 热更 / 回滚,经 CLI(§13)与 `state.json`(§7)对外。
- **容器 init 职责(常作 PID 1)**:lode 安装信号处理器(PID 1 无默认处理器),并**回收僵尸**——设为 child subreaper 并循环 `waitpid` 收割被重定父的孙进程,防僵尸堆积。`docker stop` 给 PID 1 发 `SIGTERM`(默认 10s 后 `SIGKILL`):lode 须在该 grace 内转发并退出(`stop_timeout` 应 < Docker 的 grace)。
- **信号透传(service 模式)**:lode 作为 master,把**外部收到的信号尽量透传给子进程**,不替 app 决定行为:
  - 终止类 `SIGTERM`/`SIGINT`/`SIGQUIT`:lode 发起**优雅关停**——转发给子进程 → 等其退出(超时 SIGKILL)→ 释放锁 → lode 以子进程退出码退出。
  - 透传类 `SIGHUP`/`SIGUSR1`/`SIGUSR2`/`SIGWINCH`/`SIGCONT`/`SIGTSTP`/…:**原样转发给子进程**(如 app 用 `SIGHUP` reload),lode 不消费。
  - 可选 `signals.restart`(env `LODE_RESTART_SIGNAL`,如 `SIGUSR2`):设置后该信号改为**触发 lode 优雅重启**(等价 `restart_nonce++`)且不再透传;**默认不设**,以免占用 app 信号(重启走 state.json/CLI)。
  - `SIGKILL`/`SIGSTOP` 不可捕获(OS 限制);lode 不依赖 `SIGCHLD` —— 它在约 200 ms 的监管 tick 上以非阻塞 `waitpid` 收割(同时收割被重新托管的孙进程)。透传集合可用 `signals.forward`(env `LODE_FORWARD_SIGNALS`)调整。
- **重启策略 `supervise.restart`**(`off`|`on-failure`|`always`,默认 `on-failure` —— **keep-alive**):
  - `on-failure`(默认)= **失败**(非零退出、被信号杀、或根本起不来)时按**指数退避**重启;干净 `exit(0)` 则 lode 跟随退出。**keep-alive**:重试 `restart_max` 次仍失败后 lode **不退出**,而是**暂停**(status=error)、保持存活——PID 1 绝不让容器崩溃重启。干净 `exit(0)` 仍让 lode 退出(应用主动结束)。
  - `always` = 同 `on-failure`,但干净 `exit(0)` 也重试(常驻服务不应退出);同样在上限处暂停。
  - `off` = lode **镜像子进程生命周期**——子进程自行退出时 lode 以其退出码退出(`exit(code)`→`code`;`signal(sig)`→`128+sig`),整进程重启交编排;**不**自动拉起(systemd 式监督可切回此项)。
  - 退避:基 `RESTART_BACKOFF`,封顶 `..._MAX`;`RESTART_MAX` 次后暂停(`0`=无限重试);子进程存活满 `health_grace` 重置连续计数。这些键**仅在 `restart != off` 时生效**。
  - **恢复暂停的应用**(无需退出):改 **`lode.toml`**(lode 重新加载并再次尝试——修 `[command]`/配置错误就用它;**正常运行的应用不受配置编辑干扰**)、递增 **`restart_nonce`**、或写新 **`target`**。
  - **无论何种策略**,lode 发起的转换都照常重启子进程:更新(应用 `state.target` / `policy=auto` 的更新)、回滚、显式重启(`restart_nonce` 或重启信号)。子进程退出时若有**待应用的更新**(`state.target` 指向不同的可安装版本,或 `policy=auto` 下 latest 更新),优先按更新拉起新版本而非原版重启。
- **健康/回滚**:见 §5(回滚为**单次触发**;若回滚目标本身也失败,lode 暂停而非退出)。

### 运行模式 —— 裸跑=启动,带参=透传(无需标志/子命令)

极简规则:

- **`lode`(裸跑)= 启动并监督服务**:加锁(单实例)→ 确定/安装版本(+ 必要时下载运行时)→ 跑有效 **`run`** 命令(来自 lode.toml `[command].run` 或 manifest 资产覆盖)→ 监督(按 `supervise.restart` 策略,默认 keep-alive:重试后暂停,绝不因应用失败而退出)→ 按策略轮询热更/回滚 → 信号透传如上。适合 server/daemon。镜像 `ENTRYPOINT ["/usr/bin/lode"]`,`docker run img` 即启动。
- **`lode <args...>`(带任意参数)= CLI 透传**:**不加锁、不监督、不轮询**。校验目标版本(无则引导)→ **exec 替换**为 **`exec` + `<args>`**,透传 stdin/stdout/stderr(含 TTY),信号与退出码由 OS 原生处理。
  - `lode run db:init` → `exec + ["run","db:init"]`(若 `exec="bun"` 则 ≡ `bun run db:init`)。**无需 `--`**;**不用 `run` 子命令**故不与 `bun run` 冲突。
  - 位置参数经 clap `trailing_var_arg` + `allow_hyphen_values` 透传(`lode` 无子命令,所有参数都归 app)。
  - exec 替换用安全的 `std::os::unix::process::CommandExt::exec`,保持 `#![forbid(unsafe_code)]`。
- **`lode` 本身无任何子命令**;运维与发布操作(status/update/…/keygen/sign/manifest/init)都在软链接 **`lode-cli`** 下(见 §13)。因此 `run`/`migrate` 等永远透传给 app,绝不被遮蔽。

### 重启模式 `supervise.restart_mode`

lode 以 **`stop-start`** 方式重启和更新:先停旧进程,再起新进程,有极短的空窗。它不需要 app 配合,非网络服务也适用。

`restart_mode` 还接受 `reuseport-overlap` 和 `socket-activation`,`[supervise].listen` 是为后者准备的。两者都未实现:选择其一会打印 `restart_mode is not yet supported; using stop-start`,lode 仍使用 `stop-start`。

### 就绪 / 停止握手(关键:别在 app 没准备好时就杀进程)

为避免"新进程还没起来就切流量、或旧进程还没清理完就被杀",约定两个握手:

**① 就绪握手(启动方向)`supervise.readiness`**

lode 每次拉起子进程都注入唯一实例 id **`LODE_INSTANCE`** = `{pid}-{nanoid}`(env)。app 把 `state.ready` 拼成 `{LODE_INSTANCE}-{相位}`,尾数承载握手:`-0`=可服务、`-1`=lode 的暂存升级提示、`-2`=app 的"已准备好"应答。

| `readiness` | lode 何时认为"已就绪/成功" |
|---|---|
| `none`(默认) | **存活满 `health_grace` 秒**即算就绪/good(适合无就绪信号的程序)。 |
| `state` | **等 app 写 `state.ready == "{LODE_INSTANCE}-0"`**(或裸 `{LODE_INSTANCE}`,不选用相位握手时)——app 自报"我能服务了";在 `supervise.ready_timeout`(默认 30s)内没等到 → 判失败(回滚/重启)。 |

就绪之前,lode **不**置 `status=running`,也不标 last_good。

**①′ 暂存升级的"准备"握手(`readiness=state`,选用制)**——由 app 把控切换时机。app 通过用相位 token `"{LODE_INSTANCE}-0"` 报就绪来**选用**本机制;仍用裸 `{LODE_INSTANCE}` 的 app 则不选用(lode 立即切换)。已选用的 app 被请求 `target` 后,lode 装好但**不**切换;它置 `status=updating`,并写 `state.ready="{LODE_INSTANCE}-1"` 提示**在跑的** app。app 做切换前的准备(排空在途请求、落盘、预热新数据),好了写 `state.ready="{LODE_INSTANCE}-2"` 应答,lode 这时才开始切换(§5)。等待**默认由 app 自定节奏**(`prepare_timeout = 0`);可选的非 0 `[supervise].prepare_timeout`(秒)在超时后强制切换始终不应答的 app。无论哪种,其间旧子进程一直服务且受监管(若准备途中崩溃,则因无可排空而直接应用已暂存的 target)。切换会清空 `ready`,使新实例的 `-0` 不产生歧义。

**② 停止握手(关闭方向)`supervise.stop_timeout`**

lode 发 `SIGTERM` 后,**在 `stop_timeout` 秒内绝不 SIGKILL**,给 app 充分清理时间。约定:
- operator 把 `stop_timeout` 设为 ≥ app 最坏清理耗时,且 **< 容器/编排的停止宽限**(如 Docker 默认 10s,否则容器层会先 SIGKILL)。
- app 收到 `SIGTERM` 必须尽快收尾并 `exit(0)`;只有超时才被 SIGKILL。

---

## 9. PID 锁

- 对 `$LODE_DIR/lode.pid.lock` 加非阻塞排他 `flock`,保护数据目录。监督进程在整个运行期间持有它,即使被强制杀死内核也会释放。锁文件本身从不删除。
- `$LODE_DIR/lode.pid` 以 O_EXCL(`create_new`)创建,只含 lode pid(标准 pid 文件,`kill -TERM "$(cat lode.pid)"` 可用)。退出时删除。
- `lode.pid` 已存在时,lode 探测其中记录的 pid。若是存活的 lode,说明另一个实例占用该目录,当前进程退出。进程已死、pid 已属于别的可执行文件(Linux 上通过 `/proc/<pid>/exe` 判断)、文件不可读,或记录的是 lode 自己的 pid,都视为僵尸锁并回收。
- 只有裸跑的 `lode` 加锁;`lode <args>` 透传和 `lode-cli` 不加锁。

---

## 10. 配置体系

**优先级:`CLI > 环境变量 > lode.toml 配置文件(`LODE_CONFIG`,TOML) > 默认值`。**

键名:`lode.toml` 用 snake_case(见 `docs/lode.example.toml`);环境变量用 `LODE_*`。

| 环境变量 | CLI | lode.toml 键 | 默认 | 含义 |
|---|---|---|---|---|
| `LODE_CONFIG` | `--config <path>` | — | `$LODE_DIR/lode.toml`,否则 `./lode.toml` | lode.toml 配置文件路径(TOML) |
| **`[global]`** | | | | |
| `LODE_APP_NAME` | `--app <name>` | `global.app` | `app` | 应用名(命名数据目录/锁;须与 manifest `name` 一致) |
| `LODE_DIR` | `--dir <path>` | `global.dir` | `/srv/lode` | 运行/基目录:`lode.toml` + 版本/状态/锁 + `runtime/`;默认也在此查找 `lode.toml`,缺失则自动生成一份起始配置 |
| `LODE_LOG_LEVEL` | `--log-level <lvl>` | `global.log_level` | `info` | trace/debug/info/warn/error |
| **`[update]` —— 源 + 升级策略** | | | | |
| `LODE_MANIFEST` | `--manifest <url>` | `update.manifest` | — | **native 源**:lode/v1 manifest URL(与 `github` 二选一) |
| `LODE_GITHUB` | `--github <owner/name>` | `update.github` | — | **github 源**:仓库(与 `manifest` 二选一) |
| `LODE_GITHUB_API` | `--github-api <url>` | `update.github_api` | `https://api.github.com` | (github)API 基址(GHE) |
| `LODE_ASSET` | `--asset <file>` | `update.asset` | — | 本机要装的资产**文件名**(选择键,§4/§12) |
| `LODE_CHANNEL` | `--channel <name>` | `update.channel` | `stable` | 跟随的通道(manifest 可定义多个) |
| `LODE_UPDATE_POLICY` | `--policy <off\|check\|auto>` | `update.policy` | `check` | 更新策略,§5 |
| `LODE_CHECK_INTERVAL` | `--interval <sec>` | `update.check_interval` | `300` | 检查间隔秒;0=仅启动一次 |
| `LODE_KEEP_VERSIONS` | `--keep <n>` | `update.keep_versions` | `3` | 保留旧版本数 |
| `LODE_PIN_VERSION` | `--pin <ver>` | `update.pin` | — | 锁定版本(operator) |
| **`[http]` —— 拉取凭据** | | | | |
| `LODE_HEADERS` | `--header <h>`(可重复) | `http.headers` | — | 透传给 manifest/artifact/runtime 下载的 HTTP 头(`"Name: Value"`),支持 `${ENV}` 展开,§11;该 env 变量按**换行**分隔 |
| `LODE_CREDENTIAL_HOSTS` | `--credential-host <host>` | `http.credential_hosts` | — | 接收 `[http].headers` 凭据的额外受信 host(同源始终受信;runtime 下载 host 若需凭据也需列在此);该 env 变量按**换行**分隔 |
| `LODE_ALLOW_INSECURE_HTTP` | `--allow-insecure-http` | `http.allow_insecure` | `false` | 允许明文 HTTP 拉取(loopback 始终允许;凭据将明文传输) |
| **`[trust]` —— 验签** | | | | |
| `LODE_REQUIRE_SIGNATURE` | `--require-signature <off\|auto\|enforce>` | `trust.require_signature` | `auto` | 验签强度,§6 |
| `LODE_REQUIRE_MANIFEST` | `--require-manifest <off\|signed\|fresh>` | `trust.require_manifest` | `off` | manifest 必须证明什么(签名、过期时间),§6 |
| `LODE_TRUSTED_KEYS` | `--trusted-keys <list>` | `trust.trusted_keys` | — | 受信公钥 `key_id:base64`,逗号分隔 |
| `LODE_TRUSTED_KEYS_FILE` | `--trusted-keys-file <path>` | `trust.trusted_keys_file` | — | 受信公钥文件 |
| `LODE_REVOCATION_URL` | `--revocation-url <url>` | `trust.revocation_url` | — | 委托密钥的签名吊销列表(可选),§6 |
| **`[command]` —— 怎么运行** | | | | |
| `LODE_RUN` | `--run <cmd>` | `command.run` | — | **字面裸跑启动命令**(按空白切;仅 `{dir}` 展开);manifest 资产可覆盖,见 §4 |
| `LODE_EXEC` | `--exec <cmd>` | `command.exec` | — | **字面 CLI 透传基准命令**(`lode <args>` 追加其后);manifest 资产可覆盖,见 §4 |
| `LODE_WORKDIR`(注入) | —(无 flag) | `command.workdir` | `{dir}` | 子进程 cwd(默认版本目录,或经 `[command].workdir` 设绝对路径);lode 把解析值**注入**为 `LODE_WORKDIR`,见 §4 |
| **`[env]` —— 额外子进程环境变量**(仅配置文件;无 CLI/env 覆盖) | | | | |
| — | — | `[env]`(表) | — | 注入子进程的额外环境变量,作为**默认值**——同名的宿主 env 会覆盖它(lode 自身的 `LODE_*` 高于一切),见 §4 |
| **`[runtime]` —— 可选运行时** | | | | |
| `LODE_RUNTIME` | `--runtime <name>` | `runtime.runtime` | — | 运行时可执行名(run/exec 用),见 §4 |
| `LODE_RUNTIME_DOWNLOAD` | `--runtime-download <url>` | `runtime.download` | — | 运行时缺失时的下载地址(下载后缓存复用;不做签名校验) |
| `LODE_RUNTIME_VERSION` | `--runtime-version <ver>` | `runtime.version` | — | 要求的运行时版本;探测后按子串匹配,见 §4 |
| `LODE_RUNTIME_VERSION_CHECK` | `--runtime-version-check <args>` | `runtime.version_check` | `--version` | 打印运行时版本的参数(仅在设了 `version` 时用) |
| **`[supervise]` —— 看护(重启策略/健康/回滚/停止/重启模式)** | | | | |
| `LODE_RESTART` | `--restart <off\|on-failure\|always>` | `supervise.restart` | `on-failure` | 重启策略(keep-alive):`on-failure`=失败重试后**暂停**(保活);`always`=干净退出也重试;`off`=镜像子进程(lode 随其退出),§8 |
| `LODE_RESTART_BACKOFF` | `--restart-backoff <seconds>` | `supervise.restart_backoff` | `1` | 重启退避基数(指数),单位**秒**;仅 `restart != off` 时生效 |
| `LODE_RESTART_BACKOFF_MAX` | `--restart-backoff-max <seconds>` | `supervise.restart_backoff_max` | `30` | 退避上限,单位**秒**;仅 `restart != off` 时生效 |
| `LODE_RESTART_MAX` | `--restart-max <n>` | `supervise.restart_max` | `3` | 重试上限,达到后 lode **暂停**(保活),0=无限重试;仅 `restart != off` 时生效 |
| `LODE_READINESS` | `--readiness <none\|state>` | `supervise.readiness` | `none` | 就绪判定:`none`=存活满 grace;`state`=等 app 写 `state.ready`,§8 |
| `LODE_READY_TIMEOUT` | `--ready-timeout <sec>` | `supervise.ready_timeout` | `30` | `readiness=state` 时等待就绪上限,超时判失败 |
| `LODE_HEALTH_GRACE` | `--health-grace <sec>` | `supervise.health_grace` | `15` | (readiness=none)新版需存活满此秒才算 good;亦为单次回滚的观察窗口 |
| `LODE_STOP_TIMEOUT` | `--stop-timeout <sec>` | `supervise.stop_timeout` | `10` | 优雅停超时后 SIGKILL |
| `LODE_RESTART_MODE` | `--restart-mode <mode>` | `supervise.restart_mode` | `stop-start` | 重启策略,§8(**仅实现 `stop-start`;`reuseport-overlap`/`socket-activation` 回退到它**) |
| `LODE_LISTEN` | `--listen <addr>` | `supervise.listen` | — | socket-activation 监听地址(**socket-activation 尚未实现,此键目前不生效**) |
| **`[signals]` —— 信号** | | | | |
| `LODE_FORWARD_SIGNALS` | `--forward-signals <list>` | `signals.forward` | (标准集) | 透传给子进程的信号集,§8 |
| `LODE_RESTART_SIGNAL` | `--restart-signal <sig>` | `signals.restart` | — | 触发优雅重启的信号(默认不设),§8 |

子进程环境:透传宿主环境并**剥离配置类 `LODE_*`**;把 operator 的 `[env]` 表作为**默认值**应用(仅对宿主 env 没有的 key);把运行时目录前置到 PATH;再注入只读自省变量 `LODE_ACTIVE_VERSION`、`LODE_DIR`、`LODE_WORKDIR`(解析后的 cwd)、`LODE_CONFIG`(加载的 `lode.toml` 路径,仅在加载了配置文件时)、`LODE_READINESS`(`none`|`state`)与 `LODE_INSTANCE`(`{pid}-{nanoid}`,本次启动唯一 id,由 app 拼上握手相位,§8)。优先级 低→高:`[env]` 默认值 < 继承的宿主 env < 运行时 PATH 前置 < 注入的 `LODE_*`。

---

## 11. 私有包 / 鉴权(header 列表透传)

下载 manifest 与 artifact 时,把 `headers` 列表里的 HTTP 头**原样透传**到请求——一套机制覆盖任意鉴权(Bearer、`X-Api-Key`、自定义头都行),无需为每种方案单独建模:

```toml
# lode.toml
headers = [
  "Authorization: Bearer ${RELEASE_TOKEN}",   # ${ENV} 从 lode 环境展开，密钥不落文件
  "X-Api-Key: ${API_KEY}",
]
```

- `${VAR}` 在每次请求时从 lode 进程环境展开(推荐用容器 secret/env 注入,头里不写明文)。
- artifact 设 `"auth": false` → 该 URL **不加** headers(如已自带签名的预签名链接)。
- CLI:`--header "Name: Value"`(可重复);env:`LODE_HEADERS`(换行分隔)。
- 鉴权(传输层"能下")与签名(信任层"确实是他发的",§6)正交,私有源建议都用。
- headers(含展开后的值)/私钥**永不写日志/`state.json`**;日志对 URL 查询串脱敏。

---

## 12. Manifest 格式规范(lode/v1) —— 完整约定

远程 manifest 由发布方提供,**格式为 JSON**(UTF-8),由 lode 从 `[update].manifest` 拉取,**不存本地**。**完整最大化示例见 [`docs/manifest.example.json`](./manifest.example.json)**。结构约定:

- 顶层:`schema`(必填,`"lode/v1"`)、`name`(必填,须与 `lode.toml` 的 `app` 一致)、`key_id`(可选,默认签名公钥 id)、`alg`(可选,提示性:该密钥的算法,缺省为 `ed25519`)、`sig`(可选,catalog 的签名,§6 —— *present 才验*:存在时验证,永不强制)。
- `channels`(必填):对象,键为通道名,值含 `latest`(版本 id)。**可多个通道**,lode 按 `channel` 跟随其一。
- `versions`(必填):对象,键为版本 id(被通道 `latest` 引用),值含 `notes`(可选)、`published_at`(可选,RFC 3339 时间;与 `notes` 一样仅供展示,不在签名范围内)+ `assets` 数组(≥1)。
- 每个资产以**文件名**(`name`)为键;operator 用 `[update].asset` 选其一:

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | ✓ | 资产**文件名**(如 `myapp-linux-x86_64.tar.gz`)—— 选择键与被签身份;其扩展名确定 `format`(§4) |
| `url` | ✓ | 绝对下载地址 |
| `sha256` | ✓ | 下载文件(解包前)的小写 hex 摘要 |
| `sig` | 条件 | base64 签名(密钥的算法),对 §6 消息 `(name, version, sha256, run, exec)` 签名;`require_signature=enforce` 时必填(`auto` 一旦配了受信公钥也必填) |
| `key_id` | | 覆盖顶层 `key_id` |
| `alg` | | 提示性:签名密钥的算法(缺省为 `ed25519`);验证用受信密钥自身的算法 |
| `run` | | 可选字面启动命令覆盖(已签名;覆盖 `[command].run`;见 §4) |
| `exec` | | 可选 CLI 透传命令覆盖(已签名;覆盖 `[command].exec`;见 §4) |
| `size` | | 期望字节数(多一道防护) |
| `auth` | | 默认 `true`;`false`=该 URL 不加透传 headers(预签名) |

> **无 `platform`、无 `format`、无 `kind`**:资产按文件名选,format 由扩展名推导,基础运行命令(`run`/`exec`/`workdir` + 可选 `[runtime]`)在 `lode.toml`(§4/§7/§10);manifest 资产可发布已签名的 `run`/`exec` 覆盖。manifest 声明"下载什么"(`name`/`url`/`sha256`),可选地声明"怎么启动"(`run`/`exec`),其余由 operator 决定。解压后,对有效 `run`(以及不同时的 `exec`)的第一个空白 token 做 chmod+x(条件:相对路径且解析到版本目录内的文件)。

**最小示例(单文件 JS 脚本,公开源、未签名)**:
```json
{
  "schema": "lode/v1",
  "name": "hello",
  "channels": { "stable": { "latest": "1.0.0" } },
  "versions": {
    "1.0.0": { "assets": [
      { "name": "hello.js",
        "url": "https://releases.example.com/hello-1.0.0.js",
        "sha256": "<hex>" }
    ] }
  }
}
```
（operator 设 `[update].asset = "hello.js"`。运行命令在 `lode.toml` 配:脚本用 `run = "bun run"`、`exec = "bun"`;并按需加 `[runtime]` 下载 bun。）

### 打包(发布流程,语言无关)

```bash
# 二进制（Go/Rust/bun --compile）：给资产命名,让扩展名确定 format
tar -czf myapp-linux-x86_64.tar.gz -C build myapp
lode-cli sign myapp-linux-x86_64.tar.gz --app myapp --version 1.5.0 --key publisher.key
#  → 打印 sha256 + sig + key_id(把 sig 作为 `<asset>.sig` 随资产发布,或作为 GitHub 资产 label)
lode-cli manifest myapp-linux-x86_64.tar.gz --app myapp --version 1.5.0 \
    --url https://releases.example.com/1.5.0/myapp-linux-x86_64.tar.gz \
    --run ./myapp --key publisher.key --into manifest.json   # 按 name upsert 资产(--run/--exec 可选)
lode-cli manifest-sign --into manifest.json --key publisher.key   # §6 catalog 签名

# 单文件 JS（bun build --outfile）：无打包扩展名 → raw
bun build ./src/index.ts --target bun --outfile hello.js
lode-cli sign hello.js --app hello --version 1.0.0 --key publisher.key
```

打包与签名是**发布方自理**(在各自 CI 用任意工具完成),lode 不提供打包脚本,只约定规范;流程见 [`docs/source-adapters.zh-CN.md`](source-adapters.zh-CN.md) 与 [`docs/integration.zh-CN.md`](integration.zh-CN.md)(build → sha256 → 对 `(name, version, sha256)` 签名 → 组装 manifest)。`lode-cli keygen`/`sign`/`manifest`/`manifest-sign` 为参考实现。

### Manifest 来源:native / GitHub —— 由"设了哪个 key"决定(二选一,无独立 `source`)

原生 manifest 是权威格式(显式、可签名、可放任意静态托管:S3 / OSS / GitHub raw / gh-release 资产)。为"简单通用",再支持把 **GitHub Releases** 直接当来源,适配成同一套内部模型,后续下载/校验/安装流程不变。**来源由 `[update]` 里设了哪个 key 决定**(两者互斥,都设则报错):

| 设置的 key | 来源 | lode 行为 |
|---|---|---|
| `[update].manifest = "<url>"` | native | 拉取上面规范的 `lode/v1` JSON。 |
| `[update].github = "owner/name"`(GHE 另配 `github_api`) | github | **直接用 GitHub 原生端点**,无需自己算"最新";把一个 release 映射成一个版本。 |

**channel ↔ GitHub 端点**(用 GitHub 自带的 `latest`/`tags`,不重造轮子):

| 场景 | GitHub 端点 | 说明 |
|---|---|---|
| `channel=stable` | `GET /repos/{repo}/releases/latest` | GitHub 的 `latest` = 最新的非 prerelease/非 draft release |
| `channel=beta`/其它 | `GET /repos/{repo}/releases` → 取最新 `prerelease==true` | 预发通道 |
| `pin=<tag>` | `GET /repos/{repo}/releases/tags/{tag}` | 锁定具体 tag |

**release 自身的资产即 catalog** —— **没有 `manifest.json` 资产**。适配器:
1. 按上表选出 release(latest/prerelease/tag);
2. 把每个 release 资产映射为内部资产:`name`=资产文件名、`sha256`=资产 `digest`(GitHub 计算,再对下载字节复核)、`sig`=资产无 **`label`** 时取该 release 的 **`<name>.sig`** sidecar 资产(label —— API 返回的唯一自由字符串槽 —— 存在时优先)、`url`=`browser_download_url`;
3. 之后与 native 完全相同(选 `name` 与 `[update].asset` 匹配的资产 → 校验 sha256+ed25519 → 安装)。

- **版本号**:用 release 的 `tag_name`(数字前的 `v` 前缀去掉)—— 以 GitHub 为准。以原始 tag 给出的 `pin`(或 `--version`)也解析到这个去掉 `v` 的 id,因此 `versions/<id>` 与签名绑定的 `version` 不取决于 release 是怎么被选中的 —— 发布方用去掉 `v` 的 id 签名。
- **GitHub 无 catalog(顶层)签名**:新鲜度由 tag 权威保证;每资产 `sig`(sidecar 或 label)仍保护各自下载。
- **私有 repo**:`[http].headers` 放 GitHub token(`Authorization: Bearer <PAT>`),API 与同主机资产下载都带。

> 最简用法只需 `github = "owner/repo"` + `asset = "<filename>"`,`stable` 直接走 `/releases/latest`。两源都产出相同的内部资产列表 → 同一校验/安装路径。CI 签名可选 —— 见 [`docs/source-adapters.zh-CN.md`](source-adapters.zh-CN.md) §5 的 release workflow 配方。

---

## 13. CLI —— multi-call 二进制(`lode` / `lode-cli`)

lode 是 **multi-call 二进制**,按 `argv[0]` 分流:`lode-cli` 是同一二进制的**软链接**,随二进制一同发布;镜像内 `/usr/bin/lode` 与 `/usr/bin/lode-cli` 均在 PATH 上。

```
# 以 lode 调用 = 纯加载器,无任何子命令
lode                       # 裸跑 = 启动并监督服务(跑 lode.toml 的 run)
lode <app 参数...>          # 带参 = CLI 透传(跑 exec + 参数);lode run db:init ≡ bun run db:init

# 以 lode-cli 调用(软链接)= 运维 + 发布工具箱
lode-cli <子命令> [args]

加载器 lode（无子命令,故每个参数都明确属于 app,绝不抢 app 的 run 等词）:
  (裸跑)        启动并监督服务:加锁 → 确定版本 → 跑 run → 按策略轮询热更/回滚
  <app 参数>    CLI 透传:校验版本 → exec 替换为 `exec` + 参数;不加锁/不监督

lode-cli 管理（写 state.json,与运行中的服务实例沟通）:
  status       打印 state.json + 远程 manifest 摘要后退出
  update       安装最新（或 --version <v>）；有服务在跑则写 state.json 的 target 热更，否则仅安装
  rollback     把 state.json 的 target 设为 last_good（或 --version <v>）
  restart      递增 state.json 的 restart_nonce，让服务重启子进程
  versions     列出本地已安装版本
  seed         开发/测试:把本地可执行文件/归档作为一个版本安装(无 manifest、无下载、无签名校验)并激活,使裸 `lode` 可完全离线运行它
  self-update  用最新的 lode 发布替换当前 lode 二进制,按二进制内置的发布公钥校验

lode-cli 发布/签名(发布者,详见 docs/integration.zh-CN.md):
  keygen       生成发布者密钥对 + key_id(--alg ed25519 [默认] | ecdsa-p256 | ecdsa-p384)
  sign         对 artifact 算 sha256 + 产出 sig
  verify       本地校验 artifact 的 sha256 + sig
  manifest     对单个资产签名并生成 / 合并(--into)lode/v1 manifest
  manifest-sign  就地为完整 lode/v1 manifest 写入顶层 key_id + sig(--into);--expires-in 追加已签名的过期时间
  publish      把一个版本发布到 lode-server registry:建草稿、上传制品、发布到 channel
  init         写出起始 lode.toml(示例配置)
```

全局参数即 §10 的 `--xxx`(clap 解析,带 `env` 回退),覆盖 env 与 lode.toml。

---

## 14. Crate 划分

lode 是由三个 crate 组成的 workspace,编译为同一个二进制:

| Crate | 作用 |
|---|---|
| `lode-core` | 配置、manifest 解析(两种来源)、带校验的下载与安装、状态、PID 锁,以及 `Engine` 门面。不做 CLI 解析、不处理信号,因此可嵌入。 |
| `lode-supervisor` | 监督循环:拉起、带退避的重启、信号转发、优雅停止、就绪与停止握手、热更新与回滚。信号经注入的信号源进入,因此宿主进程可以自己掌管信号。 |
| `lode` | 二进制:`lode` / `lode-cli` 多调用分发、参数解析与发布者命令。 |

进程级副作用(crypto provider、禁用 core dump、日志、panic hook)对嵌入方是可选的,通过 `InitOptions` 开启。

---

## 15. 数据目录布局

```
$LODE_DIR/
  lode.toml                # 本地配置(operator 写,app 不写;也可放别处用 --config 指)
  lode.pid                 # 持有者的 pid(标准 pid 文件)
  lode.pid.lock            # 单实例 flock;从不删除
  state.json               # 运行时状态,lode 与 app 共同写入
  state.json.lock          # 串行化 state.json 读-改-写的 flock
  revocations.json         # 已验证、合并后的吊销列表(仅在配置 [trust].revocation_url 时)
  downloads/<ver>/<asset>    # 已校验下载缓存(保留;可重新解压,随版本一起回收)
  downloads/<ver>/<asset>.part  # 下载暂存(进行中)
  runtime/<key>/<name>       # 已下载的运行时,key = <版本>-<URL 摘要>(未锁版本时为 url-<摘要>)
  versions/<ver>/            # 各版本(raw/gz 落单文件 / tar.gz/zip 解包 / binary chmod)
    .lode.json             #   该版本元数据(run/exec/format 等)供离线运行
  current -> versions/<ver>  # 原子切换的当前软链
# 注:manifest.json 在远程,不存本地。
```

---

## 16. 安全与边界

- 双层校验:sha256 + 发布者签名(ed25519 / ECDSA);`enforce` 拒绝一切未签名/验签失败。
- 失败隔离:下载与启动彻底分离。已校验的镜像包缓存在 `downloads/<ver>/<asset>` 并保留,因此解压/启动失败不丢任何东西——下次直接从缓存重新解压,无需重新下载;只有缓存缺失或校验不过才会真正去下载。中断的 `*.part` 由启动 GC 回收;缓存仅在版本 prune 时随版本一起回收。新版崩溃自动回滚——均不影响在跑版本。
- 原子性:`state.json` 以 temp+rename 写入;版本切换软链 rename,无中间态。
- 凭据:token/私钥不落日志/状态;URL 查询串脱敏;私钥只在发布侧。
- 进程:子进程用 argv 数组,不经 shell;`#![forbid(unsafe_code)]`;`rlimit` 抑制 core dump;panic hook 打结构化日志。
- 离线可用:无网时 lode 启动本地已装的 `current` 版本(其 `.lode.json` 标记,或 `lode-cli seed` 安装的版本);下一次更新检查之前不读任何 manifest。

---

## 17. app 接入

面向 app 作者:优雅退出契约、感知更新、触发升级,见 **`docs/integration.zh-CN.md`**。

