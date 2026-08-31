# simmer's direnv-compatible stdlib (subset).
#
# Sourced in the warm harness before the project's .envrc. Functions that
# direnv implements by shelling back into itself instead record their
# effect for simmer: watch_file appends to $SIMMER_WATCHES_OUT and the
# `use` family applies nix environments inline (the print-dev-env script
# ends with `eval "${shellHook:-}"`, so hooks run here too).

__simmer_expand() {
  case "$1" in
    /*) printf '%s' "$1" ;;
    *) printf '%s/%s' "$PWD" "$1" ;;
  esac
}

expand_path() { __simmer_expand "$1"; }
has() { command -v "$1" >/dev/null 2>&1; }

log_status() { printf 'simmer: %s\n' "$*" >&2; }
log_error() { printf 'simmer: %s\n' "$*" >&2; }

watch_file() {
  local f
  for f in "$@"; do
    printf '%s\n' "$(__simmer_expand "$f")" >>"$SIMMER_WATCHES_OUT"
  done
}
watch_dir() { watch_file "$@"; }

PATH_add() {
  local p
  for p in "$@"; do
    PATH="$(__simmer_expand "$p"):$PATH"
  done
  export PATH
}

path_add() {
  local var="$1" v p
  shift
  v="${!var:-}"
  for p in "$@"; do
    v="$(__simmer_expand "$p")${v:+:$v}"
  done
  export "$var=$v"
}

MANPATH_add() { path_add MANPATH "$@"; }

# Approximation: direnv parses .env files itself; `set -a` sourcing covers
# the common KEY=value form. Revisit if a real .env trips the difference.
dotenv() {
  local f="${1:-.env}"
  watch_file "$f"
  set -a
  . "$(__simmer_expand "$f")"
  set +a
}
dotenv_if_exists() {
  local f="${1:-.env}"
  [ -f "$f" ] && dotenv "$f"
  return 0
}

source_env() {
  watch_file "$1"
  . "$(__simmer_expand "$1")"
}
source_env_if_exists() {
  [ -f "$1" ] && source_env "$1"
  return 0
}

source_up() {
  local name="${1:-.envrc}" d
  d="$(dirname "$PWD")"
  while [ "$d" != "/" ]; do
    if [ -f "$d/$name" ]; then
      watch_file "$d/$name"
      # direnv evaluates the parent from its own directory.
      pushd "$d" >/dev/null && . "./$name" && popd >/dev/null
      return 0
    fi
    d="$(dirname "$d")"
  done
  return 0
}

use() {
  local kind="$1"
  shift
  if declare -F "use_$kind" >/dev/null; then
    "use_$kind" "$@"
  else
    log_error "'use $kind' is not supported"
  fi
}

use_flake() {
  # print-dev-env exports build-sandbox identity (HOME=/homeless-shelter,
  # SHELL=/noshell); restore the real ones so later .envrc lines, dotenv
  # files, and providers see a sane world.
  local __home="$HOME" __shell="$SHELL" __term="${TERM:-}"
  eval "$(nix print-dev-env --profile "$SIMMER_GCROOT" "$@")"
  export HOME="$__home" SHELL="$__shell"
  [ -n "$__term" ] && export TERM="$__term"
  return 0
}

use_nix() {
  local f="${1:-shell.nix}"
  [ -f "$f" ] || f="default.nix"
  watch_file "$f"
  local __home="$HOME" __shell="$SHELL"
  eval "$(nix print-dev-env --profile "$SIMMER_GCROOT" -f "$f")"
  export HOME="$__home" SHELL="$__shell"
  return 0
}

layout() { log_error "'layout $1' is not supported"; }
strict_env() { :; }
unstrict_env() { :; }
direnv_version() { :; }
env_vars_required() {
  local v
  for v in "$@"; do
    [ -n "${!v:-}" ] || log_error "required env var $v is unset"
  done
}
direnv_layout_dir() { printf '%s' "$SIMMER_LAYOUT_DIR"; }
