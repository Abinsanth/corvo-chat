#!/bin/sh
# Installs corvo-chat (https://github.com/Abinsanth/corvo-chat-client) on macOS or Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/Abinsanth/corvo-chat-client/release/install.sh | sh
#
# Installs the latest release to ~/.local/bin, or to $CORVO_CHAT_INSTALL_DIR
# if set, and adds that folder to PATH in your shell's profile (~/.zshrc,
# ~/.bash_profile or ~/.bashrc) unless CORVO_CHAT_NO_MODIFY_PATH is set.
# Set CORVO_CHAT_VERSION (like v0.1.0-rc.1) for a particular release,
# pre-releases included. Never uses sudo.
set -eu

REPO="Abinsanth/corvo-chat-client"
APP="corvo-chat"

fail() {
    printf '%s\n' "$APP install: $*" >&2
    exit 1
}

need() {
    command -v "$1" > /dev/null 2>&1 || fail "needs '$1', which isn't installed"
}

sha256() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum > /dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "needs 'sha256sum' or 'shasum' to verify the download"
    fi
}

# Downloads $1 to $2 (curl flag $3 sets the progress display), and says what
# went wrong in words: a time-out, GitHub's rate limit (older curls word its
# 403 differently, so the status code is read, not curl's message), or the rest.
download() {
    status=0
    code="$(curl -fSL --connect-timeout 30 "$3" -w '%{http_code}' -o "$2" "$1")" || status=$?
    [ "$status" != 0 ] || return 0
    if [ "$status" = 28 ]; then
        fail "the download timed out: $1; check your connection and try again"
    elif [ "$code" = 403 ] || [ "$code" = 429 ]; then
        fail "GitHub is limiting downloads from your network (HTTP $code); wait a while and try again: $1"
    fi
    fail "couldn't download $1"
}

# Everything runs from here, so a download cut short by `curl | sh` runs nothing.
main() {
    need curl
    need tar
    if [ -z "${CORVO_CHAT_INSTALL_DIR:-}" ] && [ -z "${HOME:-}" ]; then
        fail "HOME isn't set: set it, or CORVO_CHAT_INSTALL_DIR to the folder to install to"
    fi
    install_dir="${CORVO_CHAT_INSTALL_DIR:-$HOME/.local/bin}"
    version="${CORVO_CHAT_VERSION:-}"
    if [ -n "$version" ]; then
        release_url="https://github.com/$REPO/releases/download/$version"
    else
        release_url="https://github.com/$REPO/releases/latest/download"
    fi
    # Overridable only to test this script against a local copy of a release.
    base_url="${CORVO_CHAT_BASE_URL:-$release_url}"

    case "$(uname -s)" in
        Darwin) os=apple-darwin ;;
        Linux) os=unknown-linux-musl ;;
        *) fail "no build for $(uname -s); build from source: https://github.com/$REPO" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) fail "no build for a $(uname -m) CPU; build from source: https://github.com/$REPO" ;;
    esac
    # A shell running under Rosetta reports x86_64 on an Apple Silicon Mac.
    if [ "$os" = apple-darwin ] && [ "$(sysctl -n hw.optional.arm64 2> /dev/null)" = 1 ]; then
        arch=aarch64
    fi

    asset="$APP-$arch-$os.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    printf '%s\n' "downloading $asset"
    # A progress bar when someone's watching; logs and pipes stay quiet.
    if [ -t 2 ]; then progress=--progress-bar; else progress=--silent; fi
    download "$base_url/$asset" "$tmp/$asset" "$progress"
    download "$base_url/$asset.sha256" "$tmp/$asset.sha256" --silent
    expected="$(cut -d ' ' -f 1 < "$tmp/$asset.sha256")"
    actual="$(sha256 "$tmp/$asset")"
    [ "$expected" = "$actual" ] || fail "checksum mismatch for $asset; not installing"

    tar -xzf "$tmp/$asset" -C "$tmp"
    mkdir -p "$install_dir"
    # Copy then rename, so replacing a running corvo-chat is safe.
    cp "$tmp/$APP" "$install_dir/.$APP.new"
    chmod 755 "$install_dir/.$APP.new"
    mv -f "$install_dir/.$APP.new" "$install_dir/$APP"

    printf '%s\n' "installed $("$install_dir/$APP" --version) to $install_dir"
    add_to_path "$install_dir"
}

# The profile file your shell reads at start, or nothing for shells this
# script doesn't edit (fish and others).
profile_file() {
    case "$(basename "${SHELL:-}")" in
        zsh)
            # ZDOTDIR is often set in ~/.zshenv, which only zsh reads: ask it.
            # Its stdin is closed, and only its last line counts: a noisy
            # ~/.zshenv can print before the answer.
            zdotdir="$(zsh -c 'printf %s "${ZDOTDIR:-$HOME}"' < /dev/null 2> /dev/null | tail -n 1)" || zdotdir=""
            printf '%s\n' "${zdotdir:-${ZDOTDIR:-$HOME}}/.zshrc"
            ;;
        bash)
            if [ "$(uname -s)" != Darwin ]; then
                printf '%s\n' "$HOME/.bashrc"
                return
            fi
            # macOS terminals start login shells, which read only the first of
            # these that exists; making a new .bash_profile would hide the others.
            for f in .bash_profile .bash_login .profile; do
                if [ -f "$HOME/$f" ]; then
                    printf '%s\n' "$HOME/$f"
                    return
                fi
            done
            printf '%s\n' "$HOME/.bash_profile"
            ;;
    esac
}

# Makes `corvo-chat` work in new terminals: adds the folder to PATH in the
# shell's profile, once. Skipped with CORVO_CHAT_NO_MODIFY_PATH=1. Says
# exactly what to do by hand when it can't.
add_to_path() {
    dir="$1"
    case ":$PATH:" in
        *":$dir:"*)
            printf '%s\n' "run: $APP"
            return
            ;;
    esac
    # In single quotes, so the folder stays as it is whatever is in its name.
    line="export PATH='$(printf '%s' "$dir" | sed "s/'/'\\\\''/g")':\"\$PATH\""
    # What installers before 0.1.0 wrote.
    old_line="export PATH=\"$dir:\$PATH\""
    shell="$(basename "${SHELL:-}")"
    profile=""
    if [ -n "${HOME:-}" ]; then
        profile="$(profile_file)"
    fi
    if [ -n "$profile" ] && [ -f "$profile" ] && { grep -qxF "$line" "$profile" || grep -qxF "$old_line" "$profile"; }; then
        printf '%s\n' "$dir is already in $profile: open a new terminal, then run: $APP"
        return
    fi
    # A profile that can't be written gets the hint below, not the shell's error.
    if [ -z "${CORVO_CHAT_NO_MODIFY_PATH:-}" ] && [ -n "$profile" ] &&
        { printf '\n# added by the %s installer\n%s\n' "$APP" "$line" >> "$profile"; } 2> /dev/null; then
        echo
        printf '%s\n' "added $dir to your PATH in $profile."
        printf '%s\n' "Open a new terminal (or run: . \"$profile\"), then run: $APP"
        return
    fi
    echo
    printf '%s\n' "$dir isn't on your PATH."
    if [ "$shell" = fish ]; then
        printf '%s\n' "Run this once in fish, then open a new terminal:"
        echo
        printf '%s\n' "    fish_add_path '$(printf '%s' "$dir" | sed "s/[\\\\']/\\\\&/g")'"
    elif [ -n "$profile" ]; then
        printf '%s\n' "Add this line to $profile, then open a new terminal:"
        echo
        printf '%s\n' "    $line"
    else
        printf '%s\n' "Add this line to your shell's profile, then open a new terminal:"
        echo
        printf '%s\n' "    $line"
    fi
}

main "$@"
