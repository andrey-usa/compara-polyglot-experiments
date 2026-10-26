#!/usr/bin/env bash
# T-0069: verify + decrypt one or more CI results artifacts (encrypt-then-MAC).
# The MAC is verified BEFORE any decryption; a failed MAC aborts without output.
# Usage: decrypt-results.sh <privkey.pem> <artifact-dir> <outdir>
set -euo pipefail
PRIV="$1"; DIR="$2"; OUT="$3"
mkdir -p "$OUT"
shopt -s nullglob
found=0
for SYM in "$DIR"/*.symkey.enc; do
  found=1
  TAG=$(basename "$SYM" .symkey.enc)
  BUNDLE="$DIR/$TAG.tar.gz.enc"; MACF="$DIR/$TAG.mac"
  [ -f "$BUNDLE" ] && [ -f "$MACF" ] || { echo "MISSING-FILES $TAG" >&2; exit 1; }
  TMP=$(mktemp -d)
  openssl pkeyutl -decrypt -inkey "$PRIV" -pkeyopt rsa_padding_mode:oaep \
    -pkeyopt rsa_oaep_md:sha256 -in "$SYM" -out "$TMP/symkey.bin"
  ENCKEYHEX=$(head -c32 "$TMP/symkey.bin" | od -An -tx1 | tr -d ' \n')
  MACKEYHEX=$(tail -c32 "$TMP/symkey.bin" | od -An -tx1 | tr -d ' \n')
  cat "$SYM" "$BUNDLE" | openssl dgst -sha256 -mac HMAC -macopt hexkey:"$MACKEYHEX" -binary > "$TMP/mac.bin"
  if ! cmp -s "$TMP/mac.bin" "$MACF"; then echo "MAC-FAIL $TAG - refusing to decrypt" >&2; rm -rf "$TMP"; exit 1; fi
  IVHEX=$(head -c16 "$BUNDLE" | od -An -tx1 | tr -d ' \n')
  tail -c +17 "$BUNDLE" > "$TMP/cipher.bin"
  openssl enc -d -aes-256-cbc -K "$ENCKEYHEX" -iv "$IVHEX" -in "$TMP/cipher.bin" -out "$TMP/results.tar.gz"
  tar xzf "$TMP/results.tar.gz" -C "$OUT"
  rm -rf "$TMP"
  echo "OK $TAG -> $OUT"
done
[ "$found" = 1 ] || { echo "no *.symkey.enc artifacts in $DIR" >&2; exit 1; }
