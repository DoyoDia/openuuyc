# Linux 平台探针

这些小程序不是产品代码，也不参与构建。它们用来在真实驱动上**验证假设**，
在写 `src/platform/linux/dmabuf.rs` 之前确认零拷贝方案到底可不可行。

保留它们的原因是：零拷贝这条路径的可行性完全取决于驱动行为，而这些行为
没有文档、各家实现不同。以后换驱动、换 GPU，或者怀疑某台机器上 dmabuf 路径
为何回退到 CPU 时，用同样的探针先测一遍，比在应用里加日志再猜要快得多。

## vaapi_export_probe.c

验证 VA-API 能否把 NV12 解码表面导出为 DRM PRIME dmabuf，并打印实际布局
（fourcc、object 数、平面偏移与 pitch、DRM modifier）。

```sh
gcc -O2 -o vaapi_export_probe vaapi_export_probe.c -lva -lva-drm
./vaapi_export_probe /dev/dri/renderD128 1920 1080
```

它回答的是「第一道门槛」：驱动愿不愿意导出。零拷贝要求单 buffer、双平面的
NV12 布局；输出里 `num_objects=1` 且 `num_planes=2` 才满足，否则
`export_nv12` 会回退到 CPU 读回。

## vulkan_dmabuf_probe.c

在第一道门槛通过后，把同一个 dmabuf 用 Vulkan 导入为两个单平面 image
（Y 用 `R8Unorm`，UV 用 `R8G8Unorm`），流程与 wgpu-hal 的
`texture_from_dmabuf_fd` 一致：

```
ExternalMemoryImageCreateInfo(DMA_BUF)
+ ImageDrmFormatModifierExplicitCreateInfoEXT
+ Tiling::DRM_FORMAT_MODIFIER
+ ImportMemoryFdInfoKHR
```

```sh
gcc -O2 -o vulkan_dmabuf_probe vulkan_dmabuf_probe.c -lva -lva-drm -lvulkan
./vulkan_dmabuf_probe /dev/dri/renderD128 1920 1080
```

它回答的是「第二道门槛」：驱动导出的 DRM modifier，Vulkan 侧认不认。
两道都过，`OPENUUYC_ZEROCOPY`（默认开启）才会真正走上零拷贝。

## 已知实例

Haswell（Iris Pro P5200，i965 驱动）+ HasVK，1920x1080：

```
modifier=0x100000000000002  y_pitch=1920  uv_pitch=1920  uv_offset=2088960
[Y]  导入成功 format=9  1920x1080  offset=0       stride=1920
[UV] 导入成功 format=16 960x540    offset=2088960 stride=1920
```

两道门槛都过，所以该机器上零拷贝生效：有会话 CPU 从约 50% 降到 13%，
RSS 从 341 MB 降到 234 MB。
