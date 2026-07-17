#!/usr/bin/env sh
set -eu
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 30 \
  -keyout certs/server.key -out certs/server.crt \
  -subj '/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  -addext 'basicConstraints=critical,CA:FALSE' \
  -addext 'keyUsage=critical,digitalSignature,keyEncipherment' \
  -addext 'extendedKeyUsage=serverAuth'
chmod 600 certs/server.key
printf '%s\n' 'Created certs/server.crt and certs/server.key (development only).'
