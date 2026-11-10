if [ -d out ]; then tree -C --noreport -I 'assets.json|builds' out; else echo out/ is empty; fi
