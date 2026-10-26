#!/bin/sh
set -eu

launch_directory=$(pwd -P)
repository_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
target_directory=${CARGO_TARGET_DIR:-$repository_root/target}

umask 077
source_root="$HOME/.tickloop"
dev_root=${TICKLOOP_DEV_CONFIG_DIR:-$HOME/.tickloop-dev}
dev_parent=$(CDPATH= cd "$(dirname "$dev_root")" && pwd -P)
dev_root="$dev_parent/$(basename "$dev_root")"
if [ -d "$source_root" ]; then
    source_root=$(CDPATH= cd "$source_root" && pwd -P)
else
    source_root="$(CDPATH= cd "$HOME" && pwd -P)/.tickloop"
fi
if [ -L "$dev_root" ]; then
    echo 'Development configuration must not be a symbolic link.' >&2
    exit 1
fi
if [ -d "$dev_root" ]; then
    dev_root=$(CDPATH= cd "$dev_root" && pwd -P)
fi
case "$dev_root/" in
    "$source_root/"*) echo 'Development configuration must be outside ~/.tickloop.' >&2; exit 1 ;;
esac
if [ ! -e "$dev_root" ]; then
    mkdir "$dev_root"
    trap 'rm -rf "$dev_root"' 0
    trap 'exit 1' 1 2 15
    seeded=
    for file in settings.json auth/openai-codex-credentials.json auth/github-copilot.json \
        auth/minimax-cn.json auth/minimax-global.json auth/kimi-code-oauth.json \
        auth/xai-oauth.json auth/google-oauth.json; do
        if [ -f "$source_root/$file" ] && [ ! -L "$source_root/$file" ]; then
            case "$file" in auth/*) [ ! -L "$source_root/auth" ] || continue ;; esac
            mkdir -p "$(dirname "$dev_root/$file")"
            cp "$source_root/$file" "$dev_root/$file"
            chmod 0600 "$dev_root/$file"
            seeded="$seeded $file"
        fi
    done
    if [ -f "$source_root/.env" ] && [ ! -L "$source_root/.env" ]; then
        # Only model-provider settings; never source credentials as shell code.
        awk '/^[[:space:]]*(export[[:space:]]+)?(OPENAI_API_KEY|ANTHROPIC_API_KEY|DEEPSEEK_API_KEY|MOONSHOT_API_KEY|MOONSHOT_CN_API_KEY|ZHIPU_API_KEY|ZHIPU_CODING_API_KEY|ZAI_API_KEY|ZAI_CODING_API_KEY|MINIMAX_CN_API_KEY|MINIMAX_GLOBAL_API_KEY|DASHSCOPE_API_KEY|ALIBABA_TOKEN_PLAN_API_KEY|GEMINI_API_KEY|XAI_API_KEY|GOOGLE_APPLICATION_CREDENTIALS|GOOGLE_OAUTH_CLIENT_ID|GOOGLE_OAUTH_CLIENT_SECRET|GOOGLE_CLOUD_PROJECT|GOOGLE_CLOUD_LOCATION|JEV_API_KEY)[[:space:]]*=/' \
            "$source_root/.env" > "$dev_root/.env"
        seeded="$seeded .env (model-provider entries only)"
    fi
    trap - 0 1 2 15
    printf 'Seeded development configuration:%s\n' "${seeded:- nothing (no saved settings or model login)}" >&2
fi
export TICKLOOP_CONFIG_DIR="$dev_root"
unset TICKLOOP_TICK_RUNTIME_DIR
printf 'Development configuration: %s\n' "$TICKLOOP_CONFIG_DIR" >&2

cd "$repository_root"
mkdir -p "$repository_root/target/dev-runs"
run_directory=$(mktemp -d "$repository_root/target/dev-runs/run.XXXXXX")
bin_directory="$run_directory/bin"
trap 'rm -rf "$run_directory"' 0
trap 'exit 1' 1 2 15

cargo build --locked --target-dir "$target_directory" \
    -p tickloop-code --bin tickloop -p tickloop-tickhost --bin tickhost
mkdir "$bin_directory"
for program in tickloop tickhost; do
    cp "$target_directory/debug/$program" "$bin_directory/$program"
    chmod 0500 "$bin_directory/$program"
done

"$bin_directory/tickloop" daemon stop
"$bin_directory/tickhost" stop

# Detached daemons and workers can still exec this bundle after the View exits.
trap - 0 1 2 15
cd "$launch_directory"
if "$bin_directory/tickloop" tick list 2>/dev/null | grep -qx 'No Tick packages are installed.'; then
    echo 'No Tick packages are installed: install them from /ticks in the Agent.' >&2
fi
printf 'Development executable: %s\n' "$bin_directory/tickloop" >&2
exec "$bin_directory/tickloop" --development-root "$repository_root" "$@"
