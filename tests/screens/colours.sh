printf '\033[?1049h\033[H\033[?25l'
printf '\033[1mBold\033[0m \033[3mItalic\033[0m \033[4mUnderline\033[0m \033[2mDim\033[0m \033[7mReverse\033[0m \033[9mStrike\033[0m\n\n'
for i in 0 1 2 3 4 5 6 7; do printf "\033[4${i}m    \033[0m"; done; echo
for i in 0 1 2 3 4 5 6 7; do printf "\033[10${i}m    \033[0m"; done; echo; echo
i=0; while [ $i -lt 78 ]; do r=$((255-i*3)); g=$((i*3)); printf "\033[48;2;${r};${g};128m \033[0m"; i=$((i+1)); done; echo
i=0; while [ $i -lt 78 ]; do b=$((i*3)); printf "\033[48;2;0;${b};$((255-b))m \033[0m"; i=$((i+1)); done; echo; echo
i=16; while [ $i -lt 232 ]; do printf "\033[48;5;${i}m \033[0m"; i=$((i+1)); [ $(( (i-16) % 72 )) -eq 0 ] && echo; done; echo
i=232; while [ $i -lt 256 ]; do printf "\033[48;5;${i}m   \033[0m"; i=$((i+1)); done; echo
for c in 31 32 33 34 35 36 91 92 93 94 95 96; do printf "\033[${c}mcolour \033[0m"; done; echo
sleep 60
