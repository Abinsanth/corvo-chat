#!/bin/sh
# Runs install.sh through every case it handles, against a release served at
# $CORVO_CHAT_BASE_URL. Each case gets its own home and install folder, so
# none of them touches the real ones. Exits non-zero if any case fails.
set -eu

installer="$(cd "$(dirname "$0")/../.." && pwd)/install.sh"
: "${CORVO_CHAT_BASE_URL:?serve a release and set CORVO_CHAT_BASE_URL}"
export CORVO_CHAT_BASE_URL
root="$(mktemp -d)"
trap 'chmod -R u+w "$root"; rm -rf "$root"' EXIT
unset ZDOTDIR CORVO_CHAT_NO_MODIFY_PATH CORVO_CHAT_INSTALL_DIR
base_path="$PATH"
failed=0

# A fresh home and install folder for the case `$1`; `$2` names the folder.
start() {
    name="$1"
    HOME="$root/$name/home"
    DIR="$root/$name/${2:-bin}"
    export HOME
    mkdir -p "$HOME"
    PATH="$base_path"
    echo "== $name"
}

problem() {
    echo "FAIL ($name): $*" >&2
    failed=1
}

# Runs install.sh into $DIR with `env` settings before it; keeps what it said.
install() {
    out="$root/$name/out"
    if ! env "$@" CORVO_CHAT_INSTALL_DIR="$DIR" sh "$installer" > "$out" 2>&1; then
        problem "install.sh failed: $(cat "$out")"
        return
    fi
    "$DIR/corvo-chat" --version > /dev/null || problem "the installed app doesn't run"
}

said() {
    grep -qF -- "$1" "$out" || problem "didn't say \"$1\"; said: $(cat "$out")"
}

never_said() {
    if grep -qF -- "$1" "$out"; then problem "said \"$1\": $(cat "$out")"; fi
}

# The line install.sh puts in a profile for $DIR.
line() {
    printf "export PATH='%s':\"\$PATH\"" "$(printf '%s' "$DIR" | sed "s/'/'\\\\''/g")"
}

# How many times the PATH line is in the profile `$1`.
lines_in() {
    grep -cxF "$(line)" "$1" 2> /dev/null || true
}

bash_profile() {
    if [ "$(uname -s)" = Darwin ]; then echo "$HOME/.bash_profile"; else echo "$HOME/.bashrc"; fi
}

# Whether bash, after reading the profile `$1`, finds the installed app.
found_after() {
    test "$(bash -c ". \"\$1\"; command -v corvo-chat" _ "$1")" = "$DIR/corvo-chat" ||
        problem "bash doesn't find corvo-chat after reading $1"
}

start fresh
install SHELL=/bin/bash
profile="$(bash_profile)"
test "$(lines_in "$profile")" = 1 || problem "the PATH line isn't in $profile once"
said "added $DIR to your PATH in $profile"
found_after "$profile"

echo "== reinstall"
install SHELL=/bin/bash
test "$(lines_in "$profile")" = 1 || problem "a reinstall added the PATH line again"
said "is already in $profile"

echo "== an older installer's line"
start older
mkdir -p "$DIR"
profile="$(bash_profile)"
printf 'export PATH="%s:$PATH"\n' "$DIR" > "$profile"
install SHELL=/bin/bash
test "$(lines_in "$profile")" = 0 || problem "the new line was added beside the old one"
said "is already in $profile"

start opt-out
install SHELL=/bin/bash CORVO_CHAT_NO_MODIFY_PATH=1
profile="$(bash_profile)"
test ! -e "$profile" || problem "the profile was written"
said "Add this line to $profile"

start on-path
PATH="$DIR:$base_path"
install SHELL=/bin/bash
said "run: corvo-chat"
test ! -e "$(bash_profile)" || problem "the profile was written"

start fish
install SHELL=/usr/bin/fish
said "fish_add_path '$DIR'"
test -z "$(ls -A "$HOME")" || problem "files were written in the home: $(ls -A "$HOME")"

if command -v zsh > /dev/null 2>&1; then
    start zsh-zdotdir
    mkdir -p "$HOME/zdot"
    # Set where only zsh sees it, as people do.
    echo 'ZDOTDIR="$HOME/zdot"' > "$HOME/.zshenv"
    install SHELL="$(command -v zsh)"
    test "$(lines_in "$HOME/zdot/.zshrc")" = 1 || problem "the PATH line isn't in \$ZDOTDIR/.zshrc"
    test ! -e "$HOME/.zshrc" || problem "~/.zshrc was written instead"
    said "in $HOME/zdot/.zshrc"
else
    echo "== zsh-zdotdir: skipped, no zsh here"
fi

start odd-name "odd dir 'it's' \$HOME \`x\` (2)"
install SHELL=/bin/bash
profile="$(bash_profile)"
test "$(lines_in "$profile")" = 1 || problem "the PATH line isn't in $profile once"
found_after "$profile"

# Backslashes in a folder name: a shell's echo may read \n or \t in one as a
# newline or tab, so every message with the folder must come out as it is.
start back-slash 'back\slash \n \t \\ dir'
install SHELL=/bin/bash
profile="$(bash_profile)"
test "$(lines_in "$profile")" = 1 || problem "the PATH line isn't in $profile once"
said "added $DIR to your PATH in $profile"
found_after "$profile"
start back-slash-hint 'back\slash \n dir'
install SHELL=/bin/bash CORVO_CHAT_NO_MODIFY_PATH=1
said "    $(line)"

if [ "$(id -u)" != 0 ]; then
    start read-only-profile
    profile="$(bash_profile)"
    touch "$profile"
    chmod 444 "$profile"
    install SHELL=/bin/bash
    said "Add this line to $profile"
    never_said "ermission denied"
else
    echo "== read-only-profile: skipped, root can write anything"
fi

start no-home
install -u HOME SHELL=/bin/bash
said "isn't on your PATH"
said "Add this line to your shell's profile"

if [ "$failed" != 0 ]; then
    echo "install.sh failed some cases" >&2
    exit 1
fi
echo "install.sh passed every case"
