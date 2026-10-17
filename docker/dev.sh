#!/bin/sh
# Inner-loop entrypoint: source mounted at /src; install Django for the matrix
# leg, build the extension in debug mode, then exec the given command.
set -eu
: "${DJANGO_SPEC:=Django==6.1.*}"
. /opt/dtlvenv/bin/activate
pip install --no-cache-dir --quiet "$DJANGO_SPEC"
echo "DJANGO_SPEC=$DJANGO_SPEC $(python -c 'import django; print(django.get_version())')"
cd /src
if [ "${DTL_RELEASE:-0}" = "1" ]; then
  maturin develop --release 1>&2
else
  maturin develop 1>&2
fi
exec "$@"
