# Record the README terminal demo.
#
# From the repository root, with VHS, ttyd, ffmpeg, and `orca` on PATH:
#
#   vhs docs/demo.tape
#
# This writes docs/demo.gif. The hidden setup uses a throwaway ORCA_HOME
# (`~/.orca-demo`) and an empty `~/src/app` workspace, unsets API keys, and
# never types a prompt or a key. Re-running the tape repeats the first-run flow.

Output docs/demo.gif

Require orca

Set Shell "bash"
Set FontSize 18
Set Width 1100
Set Height 680
Set Padding 18
Set TypingSpeed 45ms
Set Theme "Catppuccin Mocha"
Set FontFamily "JetBrains Mono"
Set Framerate 20

Hide
Type "export ORCA_HOME=$HOME/.orca-demo && rm -rf $ORCA_HOME && unset DEEPSEEK_API_KEY ORCA_API_KEY && mkdir -p $HOME/src/app && cd $HOME/src/app && export PS1='$ '"
Enter
Type "clear"
Enter
Show

Type "orca"
Sleep 250ms
Enter
Sleep 2.4s
Enter
Sleep 2.8s
Escape
Sleep 400ms
