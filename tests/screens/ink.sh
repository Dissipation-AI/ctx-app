# Redraws like an Ink (React) app: synchronized output (mode 2026), cursor-up redraws.
printf '\033[?25l'
frame() {
  printf '\033[?2026h'
  [ "$1" -gt 0 ] && printf '\033[6A'
  printf '\r\033[2K╭────────────────────────────────────╮\n'
  printf '\r\033[2K│ \033[1mBuilding\033[0m %s                         │\n' "$2"
  printf '\r\033[2K│ \033[32m✔\033[0m compiled %s modules             │\n' "$3"
  printf '\r\033[2K│ \033[33m%s\033[0m                               │\n' "$4"
  printf '\r\033[2K│ > \033[2mType a message…\033[0m                  │\n'
  printf '\r\033[2K╰────────────────────────────────────╯\n'
  printf '\033[?2026l'
}
printf '\033[2J\033[H'
frame 0 '⠋' '  1' 'wait'
sleep 0.2; frame 1 '⠙' ' 40' 'wait'
sleep 0.2; frame 1 '⠹' '120' 'wait'
sleep 0.2; frame 1 '✓' '123' 'done'
sleep 60
