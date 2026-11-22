# weflow-local

按 WeFlow 本地 HTTP API 的形状提供只读数据，数据来自微信 4.x 的 `xwechat_files` 目录。补全程序通过它读最近 50 条消息和图片，不依赖微信界面。

## 安装与运行

需要 Python 3.14 x64，用当前用户默认路径安装时补全程序能找到它。在仓库根目录执行：

```powershell
$Python = Join-Path $env:LOCALAPPDATA 'Programs\Python\Python314\python.exe'
& $Python -m pip install --only-binary=:all: -r .\weflow-local\requirements.txt
```

把 [config.example.json](config.example.json) 复制成同目录的 `config.json`，填账号、数据目录、`db_key` 和令牌，令牌同时填进补全程序的「本地令牌」。`root` 用当前微信账号目录的绝对路径。

```powershell
& $Python .\weflow-local\server.py
```

默认监听 `127.0.0.1:5031`，Ctrl+C 结束。端口空闲且配置匹配时，补全程序也能在后台启动它。消息文件分片时，服务按时间合并后返回最近的记录。

## 接进补全程序

1. 右键托盘图标打开设置。
2. 「WeFlow 地址」填 `http://127.0.0.1:5031`，换过端口就填实际端口。
3. 「本地令牌」填 `config.json` 里的 `token`，两侧填同一个字符串。
4. 点「测试本地」，出现「WeFlow 已连接，私聊消息可读」即通。
5. 取消「允许读屏获取聊天历史」，「消息条数」设 50，保存。

「向模型发送图片和表情包」勾选后图片一并提交，模型需支持图片输入。

## 接口

- `GET /api/v1/health` → 状态、协议版本、账号、实际数据目录
- `GET /api/v1/sessions?keyword=&limit=` → `{"success":true,"sessions":[{"username","displayName"}]}`
- `GET /api/v1/sessions?title=&limit=` 按显示名、昵称、微信号查会话，返回 `exactTitle: true`，多个匹配时返回 409
- `GET /api/v1/messages?talker=&limit=&media=1&image=1&emoji=1&voice=0&video=0` → 含 `isSend`、`content`、`mediaType`、`mediaUrl` 的消息
- `GET /api/v1/media/<talker>/<md5>.img` → 会话附件目录中的图片，上限 2 MiB

所有接口都要令牌，`/api/v1/health` 也不例外，传 `Authorization: Bearer <token>` 或 `?access_token=`。`token` 为空时一律返回 401。

## 配置

- `port`：监听端口
- `token`：本地访问令牌
- `wxid`、`root`：账号标识与对应的 `xwechat_files` 子目录
- `db_key`：64 个十六进制字符的数据库密钥
- `cache_dir`：快照缓存目录，相对路径以配置文件所在目录为准
- `image_key`：可选，含 `xorKey` 和 `aesKey`

## 自检

绕开补全程序直接问服务：

```powershell
$t = "<config.json 里的 token>"
Invoke-RestMethod "http://127.0.0.1:5031/api/v1/health" -Headers @{Authorization="Bearer $t"} | ConvertTo-Json
Invoke-RestMethod "http://127.0.0.1:5031/api/v1/sessions?limit=5&access_token=$t" | ConvertTo-Json -Depth 3
Invoke-RestMethod "http://127.0.0.1:5031/api/v1/messages?talker=<wxid>&limit=5&access_token=$t" | ConvertTo-Json -Depth 3
```

`status` 为 `ok` 才算就绪。

## 故障

| 现象 | 处理 |
| --- | --- |
| `401 invalid token` | 两侧令牌不一致，或 `config.json` 的 `token` 为空 |
| 连接被拒绝 | 服务没启动，或端口不是填的那个 |
| 「WeFlow 服务未就绪」 | health 非 `ok`，多半是 `root`、`db_key` 不对，看 `logs\service.log` |
| 「近期私聊没有返回可读取的消息」 | 在微信里打开该会话一次再试 |
| 「WeFlow 中存在同名会话」 | 多个联系人同名，改备注使其唯一 |
| 「会话匹配结果过多」 | 标题太短，命中上百个会话 |
| 灰字里没有图片 | 模型不支持图片输入，或没勾选发送图片 |
| 服务正常但不出灰字 | 微信不在前台，或光标不在输入框 |

## 测试

装好依赖后在仓库根目录运行 `& $Python .\weflow-local\selftest.py -v`，用合成数据库和临时端口。

## 文件

- `server.py`：HTTP 层、鉴权、后台刷新
- `wfcore.py`：解密打开数据库、会话与消息查询、图片定位与解密
- `logs/service.log`：接口类别、状态码、错误类型

## 隐私

只监听 `127.0.0.1`。触发预测时，该会话最近若干条消息、草稿和风格提示词会提交给你配置的模型接口。
