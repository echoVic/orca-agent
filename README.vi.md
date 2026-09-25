# Orca

Tác nhân lập trình dành cho terminal, được thiết kế riêng cho DeepSeek.

Hãy giao cho Orca một nhiệm vụ. Orca sẽ đọc mã, chỉnh sửa tệp, chạy lệnh,
kiểm tra kết quả và tiếp tục cho đến khi hoàn thành hoặc cần quyết định của bạn.
Dùng TUI cho công việc tương tác hoặc `orca exec` cho script và CI. Orca được
viết bằng Rust, chạy cục bộ và phát hành theo giấy phép MIT.

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[Trang web](https://orcaagent.dev/) · [Nhật ký thay đổi](https://orcaagent.dev/changelog/) · [Bản phát hành](https://github.com/echoVic/orca-agent/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## Cài đặt

```bash
npm install -g @blade-ai/orca
```

Hoặc cài trực tiếp tệp nhị phân gốc:

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

Trên Windows PowerShell:

```powershell
irm https://orcaagent.dev/install.ps1 | iex
```

Trong thư mục dự án, cấu hình sandbox giới hạn bằng lệnh:

```powershell
& ([scriptblock]::Create((irm https://orcaagent.dev/install.ps1))) -SetupSandbox
```

Gói npm hỗ trợ macOS, Linux và Windows trên ARM64 và x64. Các tệp dựng sẵn cũng có tại
[GitHub Releases](https://github.com/echoVic/orca-agent/releases/latest).

## Sử dụng

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # mở TUI
orca exec "sửa bài kiểm thử đang lỗi"      # chạy không giao diện
orca exec --verifier "cargo test" "sửa nó" # kiểm tra trước khi hoàn tất
orca --mode=acp                           # kết nối máy khách ACP
```

Trên Windows PowerShell, đặt khóa bằng
`$env:DEEPSEEK_API_KEY = "sk-..."`; các lệnh `orca` vẫn giữ nguyên.

Trong TUI, `@` tìm kiếm tệp, Skills, Plugins và MCP Resources. Dùng `/plan`
để lập kế hoạch chỉ đọc, `/goal` cho mục tiêu lâu dài, `/tasks` để hiện dock tác vụ
chứa công việc nền, `/agents` để mở Agent Workspace và `/recap` để tóm tắt phiên.
Lệnh gọi công cụ cần phê duyệt hiện ngay tại vị trí ô nhập; `Shift+Tab` chuyển chế độ
phê duyệt. `/trust` quyết định Orca có nạp cấu hình và chỉ dẫn của dự án hay không; nó
không thay đổi sandbox của hệ điều hành. Xem
[hướng dẫn Terminal UI](https://orcaagent.dev/docs/#terminal-ui) (tiếng Anh và tiếng
Trung) để biết màn hình và các phím.

### Dùng Orca từ Pilion Browser

[Pilion Browser](https://github.com/echoVic/pilion-browser) là một trình duyệt desktop hoạt động như một ACP client. Chọn **Orca** trong bảng Agent của nó: Pilion khởi chạy `orca --mode=acp`, chuyển tiếp `DEEPSEEK_API_KEY`, và cung cấp các tab của chính nó cho Orca dưới dạng công cụ MCP (`browser_snapshot`, `browser_screenshot`, điều hướng, nhấp, gõ) kèm phê duyệt trước hành động và bàn giao cho con người. Bộ cài đặt cho macOS, Windows và Linux có tại [trang phát hành của Pilion](https://github.com/echoVic/pilion-browser/releases).

## Khả năng chính

- Tích hợp trực tiếp ngữ nghĩa suy luận và sử dụng công cụ của DeepSeek, với SSE,
  prompt thân thiện với bộ nhớ đệm tiền tố, quản lý ngữ cảnh tự động và thử lại.
- Đọc, tìm kiếm, chỉnh sửa và ghi mã; chạy lệnh shell; kiểm tra kết quả bằng lệnh bạn chọn.
- Kiểm soát rủi ro bằng `suggest`, `auto-edit` trong sandbox, `full-auto` toàn
  quyền, `plan` chỉ đọc và cơ chế tin cậy theo thư mục.
- Lưu lịch sử hội thoại cục bộ với hỗ trợ tiếp tục, phân nhánh, tìm kiếm, đổi tên,
  lưu trữ và nén.
- Chạy mục tiêu lâu dài không có giới hạn lượt cố định, cùng tác nhân con và
  workflow JavaScript cho các nhiệm vụ cần tiếp tục hoặc xử lý song song.
- Tải chỉ dẫn dự án, Skills, Plugins, công cụ tùy chỉnh, công cụ và tài nguyên MCP
  sau khi workspace được tin cậy.
- Cung cấp hợp đồng JSONL, app-server và Agent Client Protocol (ACP) ổn định
  cho trình soạn thảo, harness và CI.

Thứ tự ưu tiên cấu hình là biến môi trường, tham số CLI, tệp cấu hình và mặc định.
Chạy `orca --help` hoặc `orca exec --help` để xem đầy đủ lệnh. Cấu hình người dùng
nằm tại `~/.orca/config.toml`; dự án đã tin cậy cũng có thể cung cấp
`.orca/config.toml`, `AGENTS.md`, quy tắc, Skills và workflow.

Tài liệu chi tiết:

- [Persistent Goal Mode](docs/goal-mode.md)
- [Hợp đồng harness và app-server](docs/harness-contract.md)
- [Thiết kế workflow động](docs/claude-code-workflow-parity.md)
- [Lộ trình production](docs/production-roadmap.md)

## Cộng đồng

- Nhóm QQ: `472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## Đóng góp

Đọc [CONTRIBUTING.md](CONTRIBUTING.md) trước khi đóng góp. Hãy mở Issue trước
đối với thay đổi lớn hoặc có ảnh hưởng đến khả năng tương thích.

- [Báo lỗi](https://github.com/echoVic/orca-agent/issues/new?template=bug_report.yml)
- [Đề xuất tính năng](https://github.com/echoVic/orca-agent/issues/new?template=feature_request.yml)
- [Nhận hỗ trợ](SUPPORT.md)
- [Báo cáo lỗ hổng](SECURITY.md)

## Giấy phép

[MIT](LICENSE)
