//
// Copyright 2017 The Kubernetes Authors.
// Copyright (c) 2023 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

package directvolume

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"strings"

	"kata-containers/csi-kata-directvolume/pkg/utils"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"k8s.io/klog/v2"
	fs "k8s.io/kubernetes/pkg/volume/util/fs"
)

const (
	podVolumeTargetPath       = "/var/lib/kubelet/pods"
	csiSignOfVolumeTargetPath = "kubernetes.io~csi/pvc"
)

type MountPointInfo struct {
	Target              string           `json:"target"`
	Source              string           `json:"source"`
	FsType              string           `json:"fstype"`
	Options             string           `json:"options"`
	ContainerFileSystem []MountPointInfo `json:"children,omitempty"`
}

type ContainerFileSystem struct {
	Children []MountPointInfo `json:"children"`
}

type FileSystems struct {
	Filsystem []ContainerFileSystem `json:"filesystems"`
}

func parseMountInfo(originalMountInfo []byte) ([]MountPointInfo, error) {
	fs := FileSystems{
		Filsystem: make([]ContainerFileSystem, 0),
	}

	if err := json.Unmarshal(originalMountInfo, &fs); err != nil {
		return nil, err
	}

	if len(fs.Filsystem) <= 0 {
		return nil, fmt.Errorf("failed to get mount info")
	}

	return fs.Filsystem[0].Children, nil
}

func checkMountPointExist(volumePath string) (bool, error) {
	cmdPath, err := exec.LookPath("findmnt")
	if err != nil {
		return false, fmt.Errorf("findmnt not found: %w", err)
	}

	out, err := exec.Command(cmdPath, "--json").CombinedOutput()
	if err != nil {
		klog.V(3).Infof("failed to execute command: %+v", cmdPath)
		return false, err
	}

	if len(out) < 1 {
		return false, fmt.Errorf("mount point info is nil")
	}

	mountInfos, err := parseMountInfo([]byte(out))
	if err != nil {
		return false, fmt.Errorf("failed to parse the mount infos: %+v", err)
	}

	mountInfosOfPod := MountPointInfo{}
	for _, mountInfo := range mountInfos {
		if mountInfo.Target == podVolumeTargetPath {
			mountInfosOfPod = mountInfo
			break
		}
	}

	for _, mountInfo := range mountInfosOfPod.ContainerFileSystem {
		if !strings.Contains(mountInfo.Source, volumePath) {
			continue
		}

		_, err = os.Stat(mountInfo.Target)
		if err != nil {
			if os.IsNotExist(err) {
				return false, nil
			}

			return false, err
		}

		return true, nil
	}

	return false, nil
}

func (dv *directVolume) checkPVCapacityValid(volID string) (bool, error) {
	volumePath := dv.getVolumePath(volID)
	_, fscapacity, _, _, _, _, err := fs.Info(volumePath)
	if err != nil {
		klog.Errorf("failed to get capacity info: %+v", err)
		return false, err
	}

	volume, err := dv.state.GetVolumeByID(volID)
	if err != nil {
		klog.Errorf("checkPVCapacityValid failed with get volume by ID failed")
		return false, err
	}
	volumeCapacity := volume.VolSize
	klog.V(3).Infof("volume capacity: %+v fs capacity:%+v", volumeCapacity, fscapacity)

	return fscapacity >= volumeCapacity, nil
}

func getPVStats(volumePath string) (available int64, capacity int64, used int64, inodes int64, inodesFree int64, inodesUsed int64, err error) {
	return fs.Info(volumePath)
}

func (dv *directVolume) checkPVUsage(volID string) (bool, error) {
	volumePath := dv.getVolumePath(volID)
	fsavailable, _, _, _, _, _, err := fs.Info(volumePath)
	if err != nil {
		return false, err
	}

	klog.V(3).Infof("fs available: %+v", fsavailable)

	return fsavailable > 0, nil
}

func (dv *directVolume) doHealthCheckInControllerSide(volID string) (bool, error) {
	volumePath := dv.getVolumePath(volID)
	klog.V(3).Infof("Volume with ID %s has path %s.", volID, volumePath)
	spExist, err := utils.CheckPathExist(volumePath)
	if err != nil {
		return false, err
	}

	if !spExist {
		return false, status.Error(codes.NotFound, "The source path of the volume doesn't exist")
	}

	capValid, err := dv.checkPVCapacityValid(volID)
	if err != nil {
		return false, err
	}

	if !capValid {
		return false, status.Error(codes.ResourceExhausted, "The capacity of volume is greater than actual storage")
	}

	available, err := dv.checkPVUsage(volID)
	if err != nil {
		return false, err
	}

	if !available {
		return false, status.Error(codes.Unavailable, "The free space of the volume is insufficient")
	}

	return true, nil
}

func (dv *directVolume) doHealthCheckInNodeSide(volID string) (bool, error) {
	volumePath := dv.getVolumePath(volID)
	mpExist, err := checkMountPointExist(volumePath)
	if err != nil {
		return false, err
	}

	if !mpExist {
		return false, status.Error(codes.NotFound, "The volume isn't mounted")
	}

	return true, nil
}
