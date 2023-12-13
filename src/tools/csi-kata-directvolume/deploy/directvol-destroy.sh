#!/usr/bin/env bash
#
# Copyright 2017 The Kubernetes Authors.
# Copyright (c) 2023 Ant Group
#
# SPDX-License-Identifier: Apache-2.0
#

set -e
set -o pipefail

# Deleting all the resources installed by the deploy-script as the part of csi-hostpath-driver.
# Every resource in the driver installation has the label representing the installation instance.
# Using app.kubernetes.io/instance: directvolume.csi.katacontainers.io and app.kubernetes.io/part-of: csi-driver-kata-directvolume labels to identify the installation set
#
# kubectl maintains a few standard resources under "all" category which can be deleted by using just "kubectl delete all"
# and other resources such as roles, clusterrole, serivceaccount etc needs to be deleted explicitly
kubectl delete all --all-namespaces -l app.kubernetes.io/instance=directvolume.csi.katacontainers.io,app.kubernetes.io/part-of=csi-driver-kata-directvolume --wait=true
kubectl delete role,clusterrole,rolebinding,clusterrolebinding,serviceaccount,storageclass,csidriver --all-namespaces -l app.kubernetes.io/instance=directvolume.csi.katacontainers.io,app.kubernetes.io/part-of=csi-driver-kata-directvolume --wait=true
