#!/bin/bash

# SPDX-License-Identifier: MPL-2.0

set -e

SCRIPT_DIR=$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )
ASTER_SRC_DIR=${SCRIPT_DIR}/../..
DOCKER_TAG=$(cat "${ASTER_SRC_DIR}/DOCKER_IMAGE_VERSION" 2>/dev/null || cat "${ASTER_SRC_DIR}/VERSION")
IMAGE_NAME="asterinas/asterinas:${DOCKER_TAG}"

docker run -it --privileged --network=host -v /dev:/dev -v ${ASTER_SRC_DIR}:/root/asterinas ${IMAGE_NAME}
