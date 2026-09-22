# Orca

터미널을 위한 DeepSeek 네이티브 코딩 에이전트입니다.

Orca에 작업을 주면 코드를 읽고, 파일을 수정하고, 명령을 실행하고, 결과를 검증한 뒤
작업이 끝나거나 사용자의 판단이 필요할 때까지 계속 진행합니다. 대화형 작업에는 TUI를,
스크립트와 CI에는 `orca exec`를 사용할 수 있습니다. Orca는 Rust로 작성되었으며
로컬에서 실행되고 MIT 라이선스로 제공됩니다.

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[웹사이트](https://orcaagent.dev/) · [변경 기록](https://orcaagent.dev/changelog/) · [릴리스](https://github.com/echoVic/orca-agent/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## 설치

```bash
npm install -g @blade-ai/orca
```

네이티브 바이너리를 직접 설치할 수도 있습니다.

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

Windows PowerShell에서는 다음을 사용합니다.

```powershell
irm https://orcaagent.dev/install.ps1 | iex
```

프로젝트 디렉터리에서 제한된 샌드박스 capability를 설정합니다.

```powershell
& ([scriptblock]::Create((irm https://orcaagent.dev/install.ps1))) -SetupSandbox
```

npm 패키지는 macOS, Linux, Windows의 ARM64 및 x64를 지원합니다. 미리 빌드된 파일은
[GitHub Releases](https://github.com/echoVic/orca-agent/releases/latest)에서도 받을 수 있습니다.

## 사용법

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # TUI 열기
orca exec "실패한 테스트 수정"              # 헤드리스 실행
orca exec --verifier "cargo test" "수정하기" # 완료 전 검증
orca --mode=acp                           # ACP 클라이언트 연결
```

Windows PowerShell에서는 `$env:DEEPSEEK_API_KEY = "sk-..."`로 키를 설정합니다.
이후 `orca` 명령은 동일합니다.

TUI에서 `@`로 파일, Skills, Plugins, MCP Resources를 검색할 수 있습니다.
`/plan`은 읽기 전용 계획, `/goal`은 지속 목표, `/workflows`는 백그라운드 작업,
`/trust`는 현재 폴더의 샌드박스 권한을 관리합니다.

### Pilion Browser에서 Orca 사용하기

[Pilion Browser](https://github.com/echoVic/pilion-browser)는 ACP 클라이언트로 동작하는 데스크톱 브라우저입니다. Agent 패널에서 **Orca**를 선택하면 Pilion이 `orca --mode=acp`를 실행하고 `DEEPSEEK_API_KEY`를 전달하며, 자신의 탭을 MCP 도구(`browser_snapshot`, `browser_screenshot`, 이동, 클릭, 입력)로 Orca에 노출합니다. 실행 전 승인과 사람 인계도 지원합니다. macOS, Windows, Linux 설치 파일은 [Pilion 릴리스 페이지](https://github.com/echoVic/pilion-browser/releases)에서 받을 수 있습니다.

## 주요 기능

- DeepSeek의 추론 및 도구 사용 의미 체계를 직접 지원하며 SSE 스트리밍,
  프리픽스 캐시 친화적 프롬프트, 자동 컨텍스트 관리와 재시도를 제공합니다.
- 코드를 읽고, 검색하고, 수정하고, 작성하며 셸 명령과 지정한 검증 명령을 실행합니다.
- `suggest`, 샌드박스 내 `auto-edit`, 전체 접근 `full-auto`, 읽기 전용 `plan`
  모드와 폴더별 신뢰 설정으로 위험한 작업을 제어합니다.
- 로컬 대화 기록의 재개, 포크, 검색, 이름 변경, 보관 및 압축을 지원합니다.
- 고정 턴 제한이 없는 지속 목표, 서브에이전트, JavaScript 워크플로로 장기 작업을 처리합니다.
- 신뢰된 워크스페이스에서 프로젝트 지침, Skills, Plugins, 사용자 도구,
  MCP 도구와 리소스를 불러옵니다.
- 편집기, 하네스, CI를 위한 안정적인 JSONL, app-server 및 Agent Client
  Protocol(ACP) 계약을 제공합니다.

설정 우선순위는 환경 변수, CLI 인수, 설정 파일, 기본값 순서입니다.
전체 명령은 `orca --help` 또는 `orca exec --help`에서 확인할 수 있습니다.
사용자 설정은 `~/.orca/config.toml`에 있으며, 신뢰된 프로젝트는
`.orca/config.toml`, `AGENTS.md`, 규칙, Skills, 워크플로도 제공할 수 있습니다.

자세한 문서:

- [Persistent Goal Mode](docs/goal-mode.md)
- [하네스 및 app-server 계약](docs/harness-contract.md)
- [동적 워크플로 설계](docs/claude-code-workflow-parity.md)
- [프로덕션 로드맵](docs/production-roadmap.md)

## 커뮤니티

- QQ 그룹: `472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## 기여

기여하기 전에 [CONTRIBUTING.md](CONTRIBUTING.md)를 읽어 주세요. 규모가 크거나
호환성에 영향을 주는 변경은 먼저 Issue를 열어 주세요.

- [버그 신고](https://github.com/echoVic/orca-agent/issues/new?template=bug_report.yml)
- [기능 제안](https://github.com/echoVic/orca-agent/issues/new?template=feature_request.yml)
- [도움 받기](SUPPORT.md)
- [보안 취약점 신고](SECURITY.md)

## 라이선스

[MIT](LICENSE)
