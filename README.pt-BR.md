# Orca

Um agente de programação nativo para DeepSeek, feito para o seu terminal.

Dê uma tarefa ao Orca e ele lerá o código, editará arquivos, executará comandos,
verificará o resultado e continuará trabalhando até concluir ou precisar da sua
decisão. Use a TUI para trabalho interativo ou `orca exec` para scripts e CI.
O Orca é escrito em Rust, executa localmente e usa a licença MIT.

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[Site](https://orcaagent.dev/) · [Alterações](https://orcaagent.dev/changelog/) · [Versões](https://github.com/echoVic/orca-agent/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## Instalação

```bash
npm install -g @blade-ai/orca
```

Você também pode instalar o binário nativo diretamente:

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

No Windows PowerShell:

```powershell
irm https://orcaagent.dev/install.ps1 | iex
```

No diretório de um projeto, configure o sandbox restrito com:

```powershell
& ([scriptblock]::Create((irm https://orcaagent.dev/install.ps1))) -SetupSandbox
```

O pacote npm oferece suporte a macOS, Linux e Windows em ARM64 e x64. Arquivos
pré-compilados também estão disponíveis no [GitHub Releases](https://github.com/echoVic/orca-agent/releases/latest).

## Uso

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # abrir a TUI
orca exec "corrija o teste com falha"      # executar sem interface
orca exec --verifier "cargo test" "corrija" # verificar antes de concluir
orca --mode=acp                           # conectar um cliente ACP
```

No Windows PowerShell, defina a chave com
`$env:DEEPSEEK_API_KEY = "sk-..."`; os comandos `orca` são os mesmos.

Na TUI, `@` pesquisa arquivos, Skills, Plugins e MCP Resources. Use `/plan`
para planejamento somente leitura, `/goal` para um objetivo persistente,
`/workflows` para trabalhos em segundo plano e `/trust` para gerenciar as
permissões de sandbox da pasta atual.

### Usar o Orca a partir do Pilion Browser

[Pilion Browser](https://github.com/echoVic/pilion-browser) é um navegador de desktop que funciona como cliente ACP. Escolha **Orca** no painel de Agent: o Pilion inicia `orca --mode=acp`, encaminha `DEEPSEEK_API_KEY` e expõe as próprias abas ao Orca como ferramentas MCP (`browser_snapshot`, `browser_screenshot`, navegar, clicar, digitar), com aprovação antes de cada ação e transferência para o humano. Os instaladores para macOS, Windows e Linux estão na [página de versões do Pilion](https://github.com/echoVic/pilion-browser/releases).

## Principais recursos

- Usa diretamente a semântica de raciocínio e ferramentas do DeepSeek, com
  streaming SSE, prompts compatíveis com cache de prefixo, gerenciamento
  automático de contexto e novas tentativas.
- Lê, pesquisa, edita e escreve código; executa comandos shell e verifica o
  resultado com um comando escolhido por você.
- Controla ações de risco com `suggest`, `auto-edit` dentro do sandbox,
  `full-auto` com acesso total, `plan` somente leitura e confiança por pasta.
- Salva o histórico local com retomada, bifurcação, pesquisa, renomeação,
  arquivamento e compactação.
- Executa objetivos persistentes sem limite fixo de turnos, subagentes e
  workflows JavaScript para tarefas que exigem continuidade ou trabalho paralelo.
- Carrega instruções, Skills, Plugins, ferramentas personalizadas e recursos MCP
  depois que o workspace é considerado confiável.
- Fornece contratos JSONL, app-server e Agent Client Protocol (ACP) estáveis
  para editores, harnesses e CI.

A prioridade de configuração é: variáveis de ambiente, argumentos da CLI,
arquivos de configuração e valores padrão. Execute `orca --help` ou
`orca exec --help` para ver todos os comandos. A configuração do usuário fica
em `~/.orca/config.toml`; projetos confiáveis também podem fornecer
`.orca/config.toml`, `AGENTS.md`, regras, Skills e workflows.

Mais detalhes:

- [Persistent Goal Mode](docs/goal-mode.md)
- [Contrato do harness e app-server](docs/harness-contract.md)
- [Design de workflows dinâmicos](docs/claude-code-workflow-parity.md)
- [Roadmap de produção](docs/production-roadmap.md)

## Comunidade

- Grupo QQ: `472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## Como contribuir

Leia [CONTRIBUTING.md](CONTRIBUTING.md) antes de contribuir. Abra primeiro uma
Issue para alterações grandes ou que possam afetar a compatibilidade.

- [Relatar um bug](https://github.com/echoVic/orca-agent/issues/new?template=bug_report.yml)
- [Sugerir um recurso](https://github.com/echoVic/orca-agent/issues/new?template=feature_request.yml)
- [Obter ajuda](SUPPORT.md)
- [Relatar uma vulnerabilidade](SECURITY.md)

## Licença

[MIT](LICENSE)
