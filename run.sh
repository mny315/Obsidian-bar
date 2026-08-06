#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
export RUST_LOG="${RUST_LOG:-obsidian_bar=info}"

NIX_FEATURES=(--extra-experimental-features 'nix-command flakes')

print_error() {
  printf 'error: %s\n' "$*" >&2
}

have_command() {
  command -v "$1" >/dev/null 2>&1
}

confirm() {
  local prompt="$1"
  local answer

  if [[ "${OBSIDIAN_BAR_CLEAN_CONFIRM:-0}" == "1" ]]; then
    return 0
  fi

  if [[ ! -t 0 ]]; then
    print_error "$prompt Set OBSIDIAN_BAR_CLEAN_CONFIRM=1 to confirm non-interactively."
    return 1
  fi

  read -r -p "$prompt [y/N] " answer
  [[ "$answer" == "y" || "$answer" == "Y" || "$answer" == "yes" || "$answer" == "YES" ]]
}

clean_project_artifacts() {
  local bin_dir="${OBSIDIAN_BAR_BIN_DIR:-$HOME/.local/bin}"
  local command_path="$bin_dir/obsidian-bar"
  local link_target=''

  printf '\n==> removing project build artifacts\n'
  rm -rf -- "$ROOT/target" "$ROOT/result" "$ROOT"/result-* "$ROOT/run.log"

  if [[ -L "$command_path" ]]; then
    link_target="$(readlink -f -- "$command_path" 2>/dev/null || true)"
    if [[ "$link_target" == "$ROOT/target/"* || ! -e "$command_path" ]]; then
      rm -f -- "$command_path"
      printf 'removed command symlink: %s\n' "$command_path"
    fi
  fi

  printf 'project artifacts removed\n'
}

clean_app_cache() {
  local cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/obsidian-bar"

  printf '\n==> removing application cache\n'
  rm -rf -- "$cache_dir"
  printf 'removed: %s\n' "$cache_dir"
}

reset_app_state() {
  local state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/obsidian-bar"

  printf '\nThis removes saved launcher, audio, wallpaper, and bar state.\n'
  if ! confirm "Remove $state_dir?"; then
    printf 'state reset cancelled\n'
    return
  fi

  rm -rf -- "$state_dir"
  printf 'removed: %s\n' "$state_dir"
}

run_nix_gc() {
  if ! have_command nix; then
    print_error 'nix is not installed'
    return 1
  fi

  printf '\nNix garbage collection is global: it removes unreferenced store paths from all projects.\n'
  if ! confirm 'Run Nix garbage collection?'; then
    printf 'Nix garbage collection cancelled\n'
    return
  fi

  printf '\n==> nix store gc\n'
  nix "${NIX_FEATURES[@]}" store gc
}

choose_cleanup_mode() {
  case "${OBSIDIAN_BAR_CLEAN_MODE:-}" in
    safe | 1)
      printf 'safe'
      return
      ;;
    project | 2)
      printf 'project'
      return
      ;;
    cache | 3)
      printf 'cache'
      return
      ;;
    state | 4)
      printf 'state'
      return
      ;;
    nix | gc | 5)
      printf 'nix'
      return
      ;;
    all | 6)
      printf 'all'
      return
      ;;
    cancel | 0)
      printf 'cancel'
      return
      ;;
    '')
      ;;
    *)
      print_error "invalid OBSIDIAN_BAR_CLEAN_MODE=${OBSIDIAN_BAR_CLEAN_MODE}"
      exit 2
      ;;
  esac

  if [[ ! -t 0 ]]; then
    printf 'safe'
    return
  fi

  local choice
  while true; do
    printf '\nCleanup:\n' >&2
    printf '  1) Safe cleanup (project artifacts + app cache)\n' >&2
    printf '  2) Project artifacts only\n' >&2
    printf '  3) Application cache only\n' >&2
    printf '  4) Reset application state\n' >&2
    printf '  5) Nix garbage collection (global)\n' >&2
    printf '  6) Full cleanup (safe cleanup + Nix GC)\n' >&2
    printf '  0) Cancel\n' >&2
    read -r -p '> ' choice

    case "$choice" in
      1) printf 'safe'; return ;;
      2) printf 'project'; return ;;
      3) printf 'cache'; return ;;
      4) printf 'state'; return ;;
      5) printf 'nix'; return ;;
      6) printf 'all'; return ;;
      0) printf 'cancel'; return ;;
      *) printf 'Enter a number from 0 to 6.\n' >&2 ;;
    esac
  done
}

run_cleanup() {
  local mode
  mode="$(choose_cleanup_mode)"

  case "$mode" in
    safe)
      clean_project_artifacts
      clean_app_cache
      ;;
    project)
      clean_project_artifacts
      ;;
    cache)
      clean_app_cache
      ;;
    state)
      reset_app_state
      ;;
    nix)
      run_nix_gc
      ;;
    all)
      clean_project_artifacts
      clean_app_cache
      run_nix_gc
      ;;
    cancel)
      printf 'cleanup cancelled\n'
      ;;
  esac
}

choose_environment() {
  if [[ "${OBSIDIAN_BAR_DEV_ENV:-0}" == "1" || -n "${IN_NIX_SHELL:-}" ]]; then
    printf 'nix'
    return
  fi

  case "${OBSIDIAN_BAR_ENV:-}" in
    system | native | 1)
      printf 'system'
      return
      ;;
    nix | 2)
      printf 'nix'
      return
      ;;
    clean | cleanup | 3)
      printf 'clean'
      return
      ;;
    auto)
      if have_command cargo; then
        printf 'system'
      elif have_command nix; then
        printf 'nix'
      else
        print_error 'neither cargo nor nix is available'
        exit 1
      fi
      return
      ;;
    '')
      ;;
    *)
      print_error "invalid OBSIDIAN_BAR_ENV=${OBSIDIAN_BAR_ENV}"
      exit 2
      ;;
  esac

  if [[ ! -t 0 ]]; then
    if have_command cargo; then
      printf 'system'
    elif have_command nix; then
      printf 'nix'
    else
      print_error 'neither cargo nor nix is available'
      exit 1
    fi
    return
  fi

  local choice
  while true; do
    printf '\nSelect environment:\n' >&2
    printf '  1) System toolchain (cargo from PATH)\n' >&2
    printf '  2) Nix dev shell\n' >&2
    printf '  3) Cleanup\n' >&2
    read -r -p '> ' choice

    case "$choice" in
      1) printf 'system'; return ;;
      2) printf 'nix'; return ;;
      3) printf 'clean'; return ;;
      *) printf 'Enter 1, 2, or 3.\n' >&2 ;;
    esac
  done
}

choose_run_mode() {
  if [[ "${OBSIDIAN_BAR_SKIP_CHECKS:-0}" == "1" ]]; then
    printf 'run'
    return
  fi

  case "${OBSIDIAN_BAR_RUN_MODE:-}" in
    1 | check | checks)
      printf 'checks'
      return
      ;;
    2 | run | skip)
      printf 'run'
      return
      ;;
    '')
      ;;
    *)
      print_error "invalid OBSIDIAN_BAR_RUN_MODE=${OBSIDIAN_BAR_RUN_MODE}"
      exit 2
      ;;
  esac

  if [[ ! -t 0 ]]; then
    printf 'checks'
    return
  fi

  local choice
  while true; do
    printf '\nSelect launch mode:\n' >&2
    printf '  1) Run checks\n' >&2
    printf '  2) Skip checks\n' >&2
    read -r -p '> ' choice

    case "$choice" in
      1) printf 'checks'; return ;;
      2) printf 'run'; return ;;
      *) printf 'Enter 1 or 2.\n' >&2 ;;
    esac
  done
}

run_step() {
  local title="$1"
  shift
  printf '\n==> %s\n' "$title"
  "$@"
}

install_command_link() {
  local bin_dir="${OBSIDIAN_BAR_BIN_DIR:-$HOME/.local/bin}"
  local command_path="$bin_dir/obsidian-bar"
  local target="$ROOT/target/debug/obsidian-bar"

  mkdir -p "$bin_dir"

  if [[ -e "$command_path" && ! -L "$command_path" ]]; then
    printf 'warning: %s already exists and is not a symlink; leaving it unchanged\n' \
      "$command_path" >&2
    return
  fi

  ln -sfn "$target" "$command_path"
  printf 'command: %s -> %s\n' "$command_path" "$target"

  if [[ ":${PATH:-}:" != *":$bin_dir:"* ]]; then
    printf 'warning: %s is not in PATH; niri must include it to use spawn "obsidian-bar"\n' \
      "$bin_dir" >&2
  fi
}

environment="$(choose_environment)"

if [[ "$environment" == "clean" ]]; then
  run_cleanup
  exit 0
fi

run_mode="$(choose_run_mode)"

case "$environment" in
  system)
    if ! have_command cargo; then
      print_error 'cargo is not available in PATH; choose the Nix dev shell instead'
      exit 1
    fi
    ;;
  nix)
    if [[ "${OBSIDIAN_BAR_DEV_ENV:-0}" != "1" && -z "${IN_NIX_SHELL:-}" ]]; then
      if ! have_command nix; then
        print_error 'nix is not installed; choose the system toolchain instead'
        exit 1
      fi

      exec nix "${NIX_FEATURES[@]}" develop "$ROOT" --command \
        env OBSIDIAN_BAR_DEV_ENV=1 \
        OBSIDIAN_BAR_ENV=nix \
        OBSIDIAN_BAR_RUN_MODE="$run_mode" \
        "$ROOT/run.sh" "$@"
    fi
    ;;
esac

LOG_FILE="${OBSIDIAN_BAR_RUN_LOG:-$ROOT/run.log}"
mkdir -p "$(dirname -- "$LOG_FILE")"
: >"$LOG_FILE"
exec > >(tee -a "$LOG_FILE") 2>&1

printf 'obsidian-bar preflight: %s\n' "$(date --iso-8601=seconds)"
printf 'environment: %s\n' "$environment"
printf 'log: %s\n' "$LOG_FILE"

if [[ "$run_mode" == "checks" ]]; then
  run_step 'cargo fmt --check' cargo fmt --check
  run_step 'cargo check --all-targets' cargo check --all-targets
  run_step 'cargo test' cargo test
  run_step 'cargo clippy --all-targets -- -D warnings' \
    cargo clippy --all-targets -- -D warnings
else
  printf '\n==> checks skipped\n'
fi

run_step 'cargo build' cargo build
install_command_link

printf '\n==> launching obsidian-bar\n'
exec "$ROOT/target/debug/obsidian-bar" "$@"
