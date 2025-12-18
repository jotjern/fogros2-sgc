#!/bin/bash
#
# Generate a group secret for SGC.
#
# Usage: ./generate_secret.sh <name>
#
# Creates: ./secrets/<name>/secret.key (32 random bytes)
#
# All robots in a fleet must share the same secret to communicate.

set -e

if [ -z "$1" ]; then
    echo "Usage: $0 <name>"
    echo ""
    echo "Example:"
    echo "  $0 my-fleet"
    echo ""
    echo "This creates ./secrets/my-fleet/secret.key"
    exit 1
fi

NAME="$1"
SECRET_DIR="./secrets/$NAME"
SECRET_FILE="$SECRET_DIR/secret.key"

if [ -f "$SECRET_FILE" ]; then
    echo "Error: Secret already exists: $SECRET_FILE"
    echo "To regenerate, first delete: rm -rf $SECRET_DIR"
    exit 1
fi

mkdir -p "$SECRET_DIR"

# Generate 32 bytes of random data
if command -v openssl &> /dev/null; then
    openssl rand 32 > "$SECRET_FILE"
elif [ -r /dev/urandom ]; then
    head -c 32 /dev/urandom > "$SECRET_FILE"
else
    echo "Error: Cannot generate random data (no openssl or /dev/urandom)"
    exit 1
fi

chmod 600 "$SECRET_FILE"

echo "✓ Created group secret: $SECRET_FILE"
echo ""
echo "Next steps:"
echo "  1. Copy $SECRET_DIR to all robots in your fleet"
echo "  2. Set group_secret = \"$NAME\" in your config file"
echo "  3. Run: sgc check"
