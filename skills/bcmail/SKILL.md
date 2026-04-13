# BCMail

## Shared {#shared}

- 适合通过工具 `exec` 执行本地脚本 `python3 skills/bcmail/bcmail.py` 来发送邮件、查看收件箱、搜索邮件、统计未读和列出文件夹。
- 这个 skill 依赖环境变量 `BCMAIL_EMAIL` 和 `BCMAIL_PASSWORD`。缺任意一项时，先提示用户设置，再继续执行。
- 脚本会按邮箱域名自动选择服务器，当前支持：`163.com`、`vip.163.com`、`126.com`、`vip.126.com`、`188.com`、`vip.188.com`、`yeah.net`、`gmail.com`、`outlook.com`、`qq.com`。
- 绝对不能：
  - 编造未执行命令的邮件列表、邮件正文、发送结果或未读数量。
  - 回显或泄露 `BCMAIL_PASSWORD`。
  - 在收件人、主题或正文缺失时直接发送邮件。
  - 在没有明确邮件 ID 时执行 `read`。

## Prompt {#prompt}

- 工具策略：
  1. 用 `exec` 调用 `python3 skills/bcmail/bcmail.py ...`。
  2. 若命令成功且有输出，最终回复必须基于实际输出整理，不能改写成未验证的结论。
  3. 若脚本提示缺少环境变量或不支持的邮箱域名，直接说明原因，不要臆造替代方案。

- 按意图执行命令：
  - 查看最新邮件：`python3 skills/bcmail/bcmail.py inbox`
  - 查看未读邮件：`python3 skills/bcmail/bcmail.py inbox --unread`
  - 限制数量：`python3 skills/bcmail/bcmail.py inbox --limit 5`
  - 搜索邮件：`python3 skills/bcmail/bcmail.py search --keyword "关键词"`
  - 读取邮件详情：`python3 skills/bcmail/bcmail.py read --id 123`
  - 统计未读：`python3 skills/bcmail/bcmail.py unread`
  - 列出文件夹：`python3 skills/bcmail/bcmail.py folders`

- 发送邮件前置条件：
  - 必须确认 `to`、`subject`、`body` 已完整。
  - 用户要求 HTML 邮件时加 `--html`。
  - 用户提供抄送时加 `--cc "a@example.com,b@example.com"`。
  - 用户提供附件时加 `--attach file1.pdf file2.png`。

- 发送命令示例：
  - 纯文本：`python3 skills/bcmail/bcmail.py send --to "alice@example.com" --subject "会议" --body "明天下午两点开会"`
  - HTML：`python3 skills/bcmail/bcmail.py send --to "alice@example.com" --subject "日报" --html --body "<h1>日报</h1>"`
  - 含抄送和附件：`python3 skills/bcmail/bcmail.py send --to "alice@example.com" --cc "bob@example.com" --subject "报告" --attach report.pdf chart.png --body "见附件"`

- 澄清规则：
  - 用户只说“发个邮件”但没给全收件人、主题、正文时，先补齐信息。
  - 用户要“读某封邮件”但没给明确邮件 ID 时，先让用户提供邮件 ID。
  - 用户要“看邮箱”或“查未读”时，不需要额外确认，直接执行。

## Summary {#summary}

- 最终回复保留关键信息：发件人、收件人、主题、时间、未读数量或搜索命中情况。
- 发信结果要明确写“已发送”或“发送失败”，并附上脚本返回的关键信息。
- 不要输出无意义调试信息、环境变量原值或内部实现细节。
