#!/usr/bin/env bash
set -e

BASE_URL="${HSEARCH_REPO_URL:-https://hyperiondb.github.io/search}"
DIST="${HSEARCH_DIST:-stable}"
KEYRING="/usr/share/keyrings/hsearch.gpg"
LIST="/etc/apt/sources.list.d/hsearch.list"

if [ "$(id -u)" -ne 0 ]; then
  echo "this installer must run as root — pipe it to: sudo bash" >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  apt-get update
  apt-get install -y curl ca-certificates
fi

curl -fsSL "${BASE_URL}/hsearch.gpg" -o "${KEYRING}"
ARCH="$(dpkg --print-architecture)"
echo "deb [arch=${ARCH} signed-by=${KEYRING}] ${BASE_URL} ${DIST} main" > "${LIST}"
apt-get update

echo
echo "hsearch apt repository added."
echo "Install for your PostgreSQL major version, e.g.:"
echo "  apt-get install -y postgresql-18-hsearch"
