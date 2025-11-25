#!/bin/bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <path/to/elf_file> <symbol_name>" >&2
    exit 1
fi

ELF_FILE="$1"
SYMBOL_NAME="$2"

# 1. Get Symbol VA, Size, and Section Index (Ndx)
# Output fields: [1]Value [2]Size [6]Ndx
SYMBOL_DETAILS=$(readelf -Ws "$ELF_FILE" | grep -w "$SYMBOL_NAME" | head -n 1)

if [ -z "$SYMBOL_DETAILS" ]; then
    echo "Error: Symbol '$SYMBOL_NAME' not found in '$ELF_FILE'." >&2
    exit 2
fi

read -r _ SYMBOL_VA SYMBOL_SIZE _ _ _ SYMBOL_NDX _ <<< "$SYMBOL_DETAILS"

# Exit if size is zero or invalid
if ! [[ "$SYMBOL_SIZE" =~ ^[0-9]+$ ]] || [ "$SYMBOL_SIZE" -eq 0 ]; then
    echo "Error: Symbol '$SYMBOL_NAME' has an invalid or zero size ($SYMBOL_SIZE)." >&2
    exit 3
fi

# 2. Get Section VA and File Offset using Ndx
# Output fields: [4]Addr (VA) [5]Offset (File)
SECTION_LINE=$(readelf -S "$ELF_FILE" | awk -v ndx="[$SYMBOL_NDX]" '$1 == ndx {print; exit}')

if [ -z "$SECTION_LINE" ]; then
    echo "Error: Could not find section index $SYMBOL_NDX for symbol '$SYMBOL_NAME'." >&2
    exit 4
fi

read -r _ _ _ SECTION_VA SECTION_OFFSET _ <<< "$SECTION_LINE"

# 3. Calculate the Absolute File Offset (Hex-to-Dec conversion is simplified)
# We use shell arithmetic by prefixing with 16# (a Bash feature), which is cleaner than printf.

# Ensure uppercase hex digits are used for shell arithmetic (readelf uses lowercase)
SYMBOL_VA_UPPER="${SYMBOL_VA^^}"
SECTION_VA_UPPER="${SECTION_VA^^}"
SECTION_OFFSET_UPPER="${SECTION_OFFSET^^}"

# Perform arithmetic directly using Bash's 16#BASE notation
FILE_OFFSET_DEC=$(( 16#$SYMBOL_VA_UPPER - 16#$SECTION_VA_UPPER + 16#$SECTION_OFFSET_UPPER ))

# 4. Extract Content and Hash
echo "--- Symbol Details ---"
echo "Symbol: $SYMBOL_NAME"
echo "Size: $SYMBOL_SIZE bytes"
echo "File Offset (Dec): $FILE_OFFSET_DEC"
echo "----------------------"

# dd command to extract and hash the content
echo -n "Hash (SHA256): "
dd if="$ELF_FILE" bs=1 skip="$FILE_OFFSET_DEC" count="$SYMBOL_SIZE" status=none 2>/dev/null | sha256sum | awk '{print $1}'
