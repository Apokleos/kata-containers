#!/usr/bin/env bash

set -e
set -o pipefail

BASE_DIR=$(dirname "$0")

${BASE_DIR}/rbac-deploy.sh
${BASE_DIR}/directvol-deploy.sh
