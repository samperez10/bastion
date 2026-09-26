#!/data/data/com.termux/files/usr/bin/sh

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(dirname -- "$script_dir")
sound=${1:-done}

case "$sound" in
    done|attention)
        ;;
    *)
        echo "Usage: $0 [done|attention]" >&2
        exit 2
        ;;
esac

cd "$project_dir"
echo "Playing Bastion '$sound' sound through Android AAudio..."
cargo run --quiet -p terminal-lab -- sound "$sound"
echo "Sound test completed."
