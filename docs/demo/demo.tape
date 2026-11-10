Output docs/demo.gif
Set Shell bash
Set FontSize 14
Set Width 1400
Set Height 600
Set Padding 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms

Hide
Type "bash docs/demo/start.sh"
Enter
Sleep 1.5s
Type "clear"
Enter
Show

Sleep 500ms
Type "untauri list kanri | head -9"
Enter
Sleep 3.5s
Ctrl+L

Type "untauri kanri -o out --tree"
Enter
Sleep 4.5s
Ctrl+L

Type "untauri inspect kanri | head -6"
Enter
Sleep 4.5s
Ctrl+L

Type "sed -i 's|<head>|<head><title>Patched by untauri</title>|' out/index.html"
Enter
Sleep 800ms
Type "untauri repack kanri out -o kanri-patched"
Enter
Sleep 3s
Type "untauri kanri-patched -o check --tree 2>/dev/null"
Enter
Sleep 600ms
Type "grep -o '<title>.*</title>' check/index.html"
Enter
Sleep 4s
