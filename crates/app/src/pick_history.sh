# Run on the Rex server, using that account's zoxide history.
for tool in zoxide fzf; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf '%s is not installed on this server. Press Enter to close.\n' "$tool"
        IFS= read -r _
        exit 1
    fi
done
if ! history=$(zoxide query --list); then
    printf 'Could not read zoxide directory history. Press Enter to close.\n'
    IFS= read -r _
    exit 1
fi
if [ -z "$history" ]; then
    printf 'No zoxide directory history yet. Visit directories with zoxide enabled first. Press Enter to close.\n'
    IFS= read -r _
    exit 0
fi
p=$(printf '%s\n' "$history" | fzf --height=100% --layout=reverse --no-sort --no-multi --prompt='History > ') || exit 0
[ -n "$p" ] && printf '\n@@pick@@%s\n' "$p"
sleep 0.2
