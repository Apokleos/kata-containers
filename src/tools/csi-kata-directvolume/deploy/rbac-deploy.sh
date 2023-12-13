#!/usr/bin/env bash
#
# Copyright 2017 The Kubernetes Authors.
# Copyright (c) 2023 Ant Group
#
# SPDX-License-Identifier: Apache-2.0
#

set -e
set -o pipefail

BASE_DIR=$(dirname "$0")

TEMP_DIR="$( mktemp -d )"
trap 'rm -rf ${TEMP_DIR}' EXIT

# If set, the following env variables override image registry and/or tag for each of the images.
# They are named after the image name, with hyphen replaced by underscore and in upper case.
#
# - CSI_ATTACHER_REGISTRY
# - CSI_ATTACHER_TAG
# - CSI_NODE_DRIVER_REGISTRAR_REGISTRY
# - CSI_NODE_DRIVER_REGISTRAR_TAG
# - CSI_PROVISIONER_REGISTRY
# - CSI_PROVISIONER_TAG
# - CSI_SNAPSHOTTER_REGISTRY
# - CSI_SNAPSHOTTER_TAG
# - HOSTPATHPLUGIN_REGISTRY
# - HOSTPATHPLUGIN_TAG
#
# Alternatively, it is possible to override all registries or tags with:
# - IMAGE_REGISTRY
# - IMAGE_TAG
# These are used as fallback when the more specific variables are unset or empty.
#
# IMAGE_TAG=canary is ignored for images that are blacklisted in the
# deployment's optional canary-blacklist.txt file. This is meant for
# images which have known API breakages and thus cannot work in those
# deployments anymore. That text file must have the name of the blacklisted
# image on a line by itself, other lines are ignored. Example:
#
#     # The following canary images are known to be incompatible with this
#     # deployment:
#     csi-snapshotter
#
# Beware that the .yaml files do not have "imagePullPolicy: Always". That means that
# also the "canary" images will only be pulled once. This is good for testing
# (starting a pod multiple times will always run with the same canary image), but
# implies that refreshing that image has to be done manually.
#
# As a special case, 'none' as registry removes the registry name.


# The default is to use the RBAC rules that match the image that is
# being used, also in the case that the image gets overridden. This
# way if there are breaking changes in the RBAC rules, the deployment
# will continue to work.
#
# However, such breaking changes should be rare and only occur when updating
# to a new major version of a sidecar. Nonetheless, to allow testing the scenario
# where the image gets overridden but not the RBAC rules, updating the RBAC
# rules can be disabled.
: ${UPDATE_RBAC_RULES:=true}
function rbac_version () {
    yaml="$1"
    image="$2"
    update_rbac="$3"

    # get version from `image: quay.io/k8scsi/csi-attacher:v1.0.1`, ignoring comments
    version="$(sed -e 's/ *#.*$//' "$yaml" | grep "image:.*$image" | sed -e 's/ *#.*//' -e 's/.*://')"

    if $update_rbac; then
        # apply overrides
        varname=$(echo $image | tr - _ | tr a-z A-Z)
        eval version=\${${varname}_TAG:-\${IMAGE_TAG:-\$version}}
    fi

    # When using canary images, we have to assume that the
    # canary images were built from the corresponding branch.
    case "$version" in canary) version=master;;
                        *-canary) version="$(echo "$version" | sed -e 's/\(.*\)-canary/release-\1/')";;
    esac

    echo "$version"
}

# https://raw.githubusercontent.com/kubernetes-csi/external-provisioner/${VERSION}/deploy/kubernetes/rbac.yaml
CSI_PROVISIONER_RBAC_YAML="https://raw.githubusercontent.com/kubernetes-csi/external-provisioner/$(rbac_version "${BASE_DIR}kata-driectvolume/csi-hostpath-plugin.yaml" csi-provisioner false)/deploy/kubernetes/rbac.yaml"
: ${CSI_PROVISIONER_RBAC:=https://raw.githubusercontent.com/kubernetes-csi/external-provisioner/$(rbac_version "${BASE_DIR}kata-driectvolume/csi-hostpath-plugin.yaml" csi-provisioner "${UPDATE_RBAC_RULES}")/deploy/kubernetes/rbac.yaml}


run () {
    echo "$@" >&2
    "$@"
}

# rbac rules
echo "applying RBAC rules"
for component in CSI_PROVISIONER; do
    eval current="\${${component}_RBAC}"
    eval original="\${${component}_RBAC_YAML}"
    if [ "$current" != "$original" ]; then
        echo "Using non-default RBAC rules for $component. Changes from $original to $current are:"
        diff -c <(wget --quiet -O - "$original") <(if [[ "$current" =~ ^http ]]; then wget --quiet -O - "$current"; else cat "$current"; fi) || true
    fi

    # using kustomize kubectl plugin to add labels to he rbac files.
    # since we are deploying rbas directly with the url, the kustomize plugin only works with the local files
    # we need to add the files locally in temp folder and using kustomize adding labels it will be applied
    if [[ "${current}" =~ ^http:// ]] || [[ "${current}" =~ ^https:// ]]; then
      run curl "${current}" --output "${TEMP_DIR}"/rbac.yaml --silent --location
    else
        # Even for local files we need to copy because kustomize only supports files inside
        # the root of a kustomization.
        cp "${current}" "${TEMP_DIR}"/rbac.yaml
    fi

    cat <<- EOF > "${TEMP_DIR}"/kustomization.yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization

commonLabels:
  app.kubernetes.io/instance: directvolume.csi.katacontainers.io
  app.kubernetes.io/part-of: csi-driver-kata-directvolume

resources:
- ./rbac.yaml
EOF

    run kubectl apply --kustomize "${TEMP_DIR}"
done
