//
// Copyright 2017 The Kubernetes Authors.
// Copyright (c) 2023 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

package directvolume

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"

	"kata-containers/csi-kata-directvolume/pkg/state"
	"kata-containers/csi-kata-directvolume/pkg/utils"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"golang.org/x/net/context"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"k8s.io/klog/v2"
)

const (
	TopologyKeyNode = "topology.directvolume.csi/node"

	failedPreconditionAccessModeConflict = "volume uses SINGLE_NODE_SINGLE_WRITER access mode and is already mounted at a different target path"
)

func (dv *directVolume) NodePublishVolume(ctx context.Context, req *csi.NodePublishVolumeRequest) (*csi.NodePublishVolumeResponse, error) {
	klog.V(4).Infof("node publish volume with request %v", req)

	// Check arguments
	if req.GetVolumeCapability() == nil {
		return nil, status.Error(codes.InvalidArgument, "Volume capability missing in request")
	}
	if len(req.GetVolumeId()) == 0 {
		return nil, status.Error(codes.InvalidArgument, "Volume ID missing in request")
	}
	if len(req.GetTargetPath()) == 0 {
		return nil, status.Error(codes.InvalidArgument, "Target path missing in request")
	}
	if !isDirectVolume(req.VolumeContext) {
		return nil, status.Errorf(codes.FailedPrecondition, "volume %q is not direct-volume.", req.VolumeId)
	}

	targetPath := req.GetTargetPath()
	if req.GetVolumeCapability().GetMount() == nil {
		return nil, status.Error(codes.InvalidArgument, "It Must be mount access type")
	}

	dv.mutex.Lock()
	defer dv.mutex.Unlock()

	fsType := req.VolumeContext[utils.KataContainersDirectFsType]
	if len(fsType) == 0 {
		fsType = utils.DefaultFsType
		klog.Warningf("volume context has no fsType, set default fstype %v\n", fsType)
	}

	readOnly := req.GetReadonly()
	volumeId := req.GetVolumeId()
	attrib := req.GetVolumeContext()
	mountFlags := req.GetVolumeCapability().GetMount().GetMountFlags()

	devicePath := dv.config.VolumeDevices[volumeId]
	klog.Infof("target %v\nfstype %v\ndevice %v\nreadonly %v\nvolumeId %v\nattributes %v\nmountflags %v\n",
		targetPath, fsType, devicePath, readOnly, volumeId, attrib, mountFlags)

	options := []string{"bind"}
	if readOnly {
		options = append(options, "ro")
	} else {
		options = append(options, "rw")
	}

	stagingTargetPath := req.GetStagingTargetPath()

	if canDoMnt, err := utils.CanDoBindMount(dv.config.safeMounter, targetPath); err != nil {
		return nil, err
	} else if !canDoMnt {
		klog.V(3).Infof("cannot do bind mount target path: %s", targetPath)
		return &csi.NodePublishVolumeResponse{}, nil
	}

	if err := dv.config.safeMounter.DoBindMount(stagingTargetPath, targetPath, "", options); err != nil {
		errMsg := fmt.Sprintf("failed to mount device: %s at %s: %s", stagingTargetPath, targetPath, err.Error())
		klog.Infof("do bind mount failed: %v.", errMsg)
		return nil, status.Error(codes.Aborted, errMsg)
	}

	// kata-containers DirectVolume add
	mountInfo := utils.MountInfo{
		VolumeType: "directvol",
		Device:     devicePath,
		FsType:     fsType,
		Metadata:   attrib,
		Options:    options,
	}
	if err := utils.AddDirectVolume(targetPath, mountInfo); err != nil {
		klog.Errorf("add direct volume with source %s and mountInfo %v failed", targetPath, mountInfo)
		return nil, err
	}
	klog.Infof("add direct volume successfully.")

	var vol state.Volume
	var err error
	if vol, err = dv.state.GetVolumeByID(volumeId); err != nil {
		capInt64, _ := strconv.ParseInt(req.VolumeContext[utils.CapabilityInBytes], 10, 64)
		volName := req.VolumeContext[utils.DirectVolumeName]
		kind := req.VolumeContext[storageKind]
		vol, _ := dv.createVolume(volumeId, volName, capInt64, kind)
		vol.NodeID = dv.config.NodeID
		vol.Published.Add(targetPath)
		klog.Infof("create volume %v successfully", vol)

		return &csi.NodePublishVolumeResponse{}, nil
	}

	vol.NodeID = dv.config.NodeID
	vol.Published.Add(targetPath)
	if err := dv.state.UpdateVolume(vol); err != nil {
		return nil, err
	}

	klog.Infof("directvolume: volume %s has been published.", targetPath)

	return &csi.NodePublishVolumeResponse{}, nil
}

func (dv *directVolume) NodeUnpublishVolume(ctx context.Context, req *csi.NodeUnpublishVolumeRequest) (*csi.NodeUnpublishVolumeResponse, error) {

	// Check arguments
	if len(req.GetVolumeId()) == 0 {
		return nil, status.Error(codes.InvalidArgument, "Volume ID missing in request")
	}
	if len(req.GetTargetPath()) == 0 {
		return nil, status.Error(codes.InvalidArgument, "Target path missing in request")
	}

	targetPath := req.GetTargetPath()
	volumeID := req.GetVolumeId()

	// Lock before acting on global state. A production-quality
	// driver might use more fine-grained locking.
	dv.mutex.Lock()
	defer dv.mutex.Unlock()

	// Unmount only if the target path is really a mount point.
	if isMnt, err := dv.config.safeMounter.IsMountPoint(targetPath); err != nil {
		return nil, fmt.Errorf("check target path: %w", err)
	} else if isMnt {
		// Unmounting the image or filesystem.
		err = dv.config.safeMounter.Unmount(targetPath)
		if err != nil {
			return nil, fmt.Errorf("unmount target path: %w", err)
		}
	}

	// Delete the mount point.
	// Does not return error for non-existent path, repeated calls OK for idempotency.
	if err := os.RemoveAll(targetPath); err != nil {
		return nil, fmt.Errorf("remove target path: %w", err)
	}

	if err := utils.RemoveDirectVolume(targetPath); err != nil {
		klog.V(4).Infof("remove direct volume failed.")
		utils.RemoveDirectVolume(targetPath)
	}

	klog.Infof("direct volume %s has been cleaned up.", targetPath)

	vol, err := dv.state.GetVolumeByID(volumeID)
	if err != nil {
		klog.Warningf("volume id %s not found in volume list, nothing to do.")
		return &csi.NodeUnpublishVolumeResponse{}, nil
	}

	if !vol.Published.Has(targetPath) {
		klog.V(4).Infof("volume %q is not published at %q, nothing to do.", volumeID, targetPath)
		return &csi.NodeUnpublishVolumeResponse{}, nil
	}

	vol.Published.Remove(targetPath)
	if err := dv.state.UpdateVolume(vol); err != nil {
		return nil, err
	}
	klog.Infof("volume %s has been unpublished.", targetPath)

	return &csi.NodeUnpublishVolumeResponse{}, nil
}

func isDirectVolume(VolumeCtx map[string]string) bool {
	if VolumeCtx[utils.IsDirectVolume] == "True" {
		return true
	}

	return false
}

func (dv *directVolume) NodeStageVolume(ctx context.Context, req *csi.NodeStageVolumeRequest) (*csi.NodeStageVolumeResponse, error) {
	klog.V(4).Infof("NodeStageVolumeRequest with request %v", req)

	volumeID := req.GetVolumeId()
	// Check arguments
	if len(volumeID) == 0 {
		return nil, status.Error(codes.InvalidArgument, "Volume ID missing in request")
	}
	stagingTargetPath := req.GetStagingTargetPath()
	if stagingTargetPath == "" {
		return nil, status.Error(codes.InvalidArgument, "Target path missing in request")
	}
	if req.GetVolumeCapability() == nil {
		return nil, status.Error(codes.InvalidArgument, "Volume Capability missing in request")
	}

	if !isDirectVolume(req.VolumeContext) {
		return nil, status.Errorf(codes.FailedPrecondition, "volume %q is not direct-volume.", req.VolumeId)
	}

	// Lock before acting on global state. A production-quality
	// driver might use more fine-grained locking.
	dv.mutex.Lock()
	defer dv.mutex.Unlock()
	// Create direct block device and mkfs with fsType, default fsType is "ext4".

	capacityInBytes := req.VolumeContext[utils.CapabilityInBytes]
	devicePath, err := utils.CreateDirectBlockDevice(volumeID, capacityInBytes, dv.config.StoragePath)
	if err != nil {
		errMsg := status.Errorf(codes.Internal, "setup storage for volume '%s' failed", volumeID)
		return &csi.NodeStageVolumeResponse{}, errMsg
	}

	// /full_path_on_host/VolumeId/
	deviceUpperPath := filepath.Dir(*devicePath)
	if canMnt, err := utils.CanDoBindMount(dv.config.safeMounter, stagingTargetPath); err != nil {
		return nil, err
	} else if !canMnt {
		klog.Infof("staging target path: %s already mounted", stagingTargetPath)
		return &csi.NodeStageVolumeResponse{}, nil
	}

	options := []string{"bind"}
	if err := dv.config.safeMounter.DoBindMount(deviceUpperPath, stagingTargetPath, "", options); err != nil {
		klog.Errorf("safe mounter: %v do bind mount %v failed, with error: %v", deviceUpperPath, stagingTargetPath, err.Error())
		return nil, err
	}

	fsType, ok := req.VolumeContext[utils.KataContainersDirectFsType]
	if !ok {
		klog.Infof("fstype not set, default fstype will be set: %v\n", utils.DefaultFsType)
		fsType = utils.DefaultFsType
	}

	if err := dv.config.safeMounter.SafeFormatWithFstype(*devicePath, fsType, options); err != nil {
		return nil, err
	}

	dv.config.VolumeDevices[volumeID] = *devicePath

	klog.Infof("directvolume: volume %s has been staged.", stagingTargetPath)

	volInStat, err := dv.state.GetVolumeByID(req.VolumeId)
	if err != nil {
		capInt64, _ := strconv.ParseInt(req.VolumeContext[utils.CapabilityInBytes], 10, 64)
		volName := req.VolumeContext[utils.DirectVolumeName]
		kind := req.VolumeContext[storageKind]
		vol, _ := dv.createVolume(volumeID, volName, capInt64, kind)
		vol.Staged.Add(stagingTargetPath)

		klog.Infof("create volume %v successfully", vol)
		return &csi.NodeStageVolumeResponse{}, nil
	}

	if volInStat.Staged.Has(stagingTargetPath) {
		klog.V(4).Infof("Volume %q is already staged at %q, nothing to do.", req.VolumeId, stagingTargetPath)
		return &csi.NodeStageVolumeResponse{}, nil
	}

	if !volInStat.Staged.Empty() {
		return nil, status.Errorf(codes.FailedPrecondition, "volume %q is already staged at %v", req.VolumeId, volInStat.Staged)
	}

	volInStat.Staged.Add(stagingTargetPath)
	if err := dv.state.UpdateVolume(volInStat); err != nil {
		return nil, err
	}

	return &csi.NodeStageVolumeResponse{}, nil
}

func (dv *directVolume) NodeUnstageVolume(ctx context.Context, req *csi.NodeUnstageVolumeRequest) (*csi.NodeUnstageVolumeResponse, error) {
	// Check arguments
	volumeID := req.GetVolumeId()
	if len(volumeID) == 0 {
		return nil, status.Error(codes.InvalidArgument, "Volume ID missing in request")
	}
	stagingTargetPath := req.GetStagingTargetPath()
	if stagingTargetPath == "" {
		return nil, status.Error(codes.InvalidArgument, "Target path missing in request")
	}

	// Lock before acting on global state. A production-quality
	// driver might use more fine-grained locking.
	dv.mutex.Lock()
	defer dv.mutex.Unlock()

	// Unmount only if the target path is really a mount point.
	if isMnt, err := dv.config.safeMounter.IsMountPoint(stagingTargetPath); err != nil {
		return nil, fmt.Errorf("check staging target path: %w", err)
	} else if isMnt {
		err = dv.config.safeMounter.Unmount(stagingTargetPath)
		if err != nil {
			return nil, fmt.Errorf("unmount staging target path: %w", err)
		}
	}

	if deviceUpperPath, err := utils.GetStoragePath(dv.config.StoragePath, volumeID); err != nil {
		return nil, fmt.Errorf("get device UpperPath %s failed: %w", deviceUpperPath, err)
	} else {
		if err = os.RemoveAll(deviceUpperPath); err != nil {
			return nil, fmt.Errorf("remove device upper path: %s failed %w", deviceUpperPath, err)
		}
		klog.Infof("direct volume %s has been removed.", deviceUpperPath)
	}

	if err := os.RemoveAll(stagingTargetPath); err != nil {
		return nil, fmt.Errorf("remove staging target path: %w", err)
	}

	klog.Infof("directvolume: volume %s has been unstaged.", stagingTargetPath)
	vol, err := dv.state.GetVolumeByID(volumeID)
	if err != nil {
		klog.Warning("Volume not found: might have already deleted")
		return &csi.NodeUnstageVolumeResponse{}, nil
	}

	if !vol.Staged.Has(stagingTargetPath) {
		klog.V(4).Infof("Volume %q is not staged at %q, nothing to do.", volumeID, stagingTargetPath)
		return &csi.NodeUnstageVolumeResponse{}, nil
	}

	if !vol.Published.Empty() {
		return nil, status.Errorf(codes.Internal, "volume %q is still published at %q on node %q", vol.VolID, vol.Published, vol.NodeID)
	}

	vol.Staged.Remove(stagingTargetPath)
	if err := dv.state.UpdateVolume(vol); err != nil {
		return nil, err
	}

	return &csi.NodeUnstageVolumeResponse{}, nil
}

func (dv *directVolume) NodeGetInfo(ctx context.Context, req *csi.NodeGetInfoRequest) (*csi.NodeGetInfoResponse, error) {
	resp := &csi.NodeGetInfoResponse{
		NodeId: dv.config.NodeID,
	}

	if dv.config.EnableTopology {
		resp.AccessibleTopology = &csi.Topology{
			Segments: map[string]string{TopologyKeyNode: dv.config.NodeID},
		}
	}

	return resp, nil
}

func (dv *directVolume) NodeGetCapabilities(ctx context.Context, req *csi.NodeGetCapabilitiesRequest) (*csi.NodeGetCapabilitiesResponse, error) {
	caps := []*csi.NodeServiceCapability{
		{
			Type: &csi.NodeServiceCapability_Rpc{
				Rpc: &csi.NodeServiceCapability_RPC{
					Type: csi.NodeServiceCapability_RPC_STAGE_UNSTAGE_VOLUME,
				},
			},
		},
		{
			Type: &csi.NodeServiceCapability_Rpc{
				Rpc: &csi.NodeServiceCapability_RPC{
					Type: csi.NodeServiceCapability_RPC_VOLUME_CONDITION,
				},
			},
		},
		{
			Type: &csi.NodeServiceCapability_Rpc{
				Rpc: &csi.NodeServiceCapability_RPC{
					Type: csi.NodeServiceCapability_RPC_GET_VOLUME_STATS,
				},
			},
		},
		{
			Type: &csi.NodeServiceCapability_Rpc{
				Rpc: &csi.NodeServiceCapability_RPC{
					Type: csi.NodeServiceCapability_RPC_SINGLE_NODE_MULTI_WRITER,
				},
			},
		},
	}

	return &csi.NodeGetCapabilitiesResponse{Capabilities: caps}, nil
}

func (dv *directVolume) NodeGetVolumeStats(ctx context.Context, in *csi.NodeGetVolumeStatsRequest) (*csi.NodeGetVolumeStatsResponse, error) {
	if len(in.GetVolumeId()) == 0 {
		return nil, status.Error(codes.InvalidArgument, "Volume ID not provided")
	}
	if len(in.GetVolumePath()) == 0 {
		return nil, status.Error(codes.InvalidArgument, "Volume Path not provided")
	}

	// Lock before acting on global state. A production-quality
	// driver might use more fine-grained locking.
	dv.mutex.Lock()
	defer dv.mutex.Unlock()

	volume, err := dv.state.GetVolumeByID(in.GetVolumeId())
	if err != nil {
		klog.Errorf("Node get volume stat failed when get volume by ID")
		return nil, err
	}

	if _, err := os.Stat(in.GetVolumePath()); err != nil {
		return nil, status.Errorf(codes.NotFound, "Could not get file information from %s: %+v", in.GetVolumePath(), err)
	}

	healthy, msg := dv.doHealthCheckInNodeSide(in.GetVolumeId())
	klog.V(3).Infof("Healthy state: %+v Volume: %+v", volume.VolName, healthy)
	available, capacity, used, inodes, inodesFree, inodesUsed, err := getPVStats(in.GetVolumePath())
	if err != nil {
		return nil, fmt.Errorf("get volume stats failed: %w", err)
	}

	klog.V(3).Infof("Capacity: %+v Used: %+v Available: %+v Inodes: %+v Free inodes: %+v Used inodes: %+v", capacity, used, available, inodes, inodesFree, inodesUsed)
	return &csi.NodeGetVolumeStatsResponse{
		Usage: []*csi.VolumeUsage{
			{
				Available: available,
				Used:      used,
				Total:     capacity,
				Unit:      csi.VolumeUsage_BYTES,
			}, {
				Available: inodesFree,
				Used:      inodesUsed,
				Total:     inodes,
				Unit:      csi.VolumeUsage_INODES,
			},
		},
		VolumeCondition: &csi.VolumeCondition{
			Abnormal: !healthy,
			Message:  msg.Error(),
		},
	}, nil
}

// NodeExpandVolume is only implemented so the driver can be used for e2e testing.
func (dv *directVolume) NodeExpandVolume(ctx context.Context, req *csi.NodeExpandVolumeRequest) (*csi.NodeExpandVolumeResponse, error) {

	return &csi.NodeExpandVolumeResponse{}, nil
}

// makeFile ensures that the file exists, creating it if necessary.
// The parent directory must exist.
func makeFile(pathname string) error {
	f, err := os.OpenFile(pathname, os.O_CREATE, os.FileMode(0644))
	defer f.Close()
	if err != nil {
		if !os.IsExist(err) {
			return err
		}
	}
	return nil
}

// hasSingleNodeSingleWriterAccessMode checks if the publish request uses the
// SINGLE_NODE_SINGLE_WRITER access mode.
func hasSingleNodeSingleWriterAccessMode(req *csi.NodePublishVolumeRequest) bool {
	accessMode := req.GetVolumeCapability().GetAccessMode()
	return accessMode != nil && accessMode.GetMode() == csi.VolumeCapability_AccessMode_SINGLE_NODE_SINGLE_WRITER
}

// isMountedElsewhere checks if the volume to publish is mounted elsewhere on
// the node.
func isMountedElsewhere(req *csi.NodePublishVolumeRequest, vol state.Volume) bool {
	for _, targetPath := range vol.Published {
		if targetPath != req.GetTargetPath() {
			return true
		}
	}
	return false
}
