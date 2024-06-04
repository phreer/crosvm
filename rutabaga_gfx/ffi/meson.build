# Copyright 2024 The ChromiumOS Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

project('rutabaga_gfx_ffi', ['rust', 'c'],
        version: files('VERSION'))

target_os = host_machine.system()

# By default cargo would generate rutabaga_gfx_ffi.dll (without the lib
# prefix) for a Windows cdylib
if target_os == 'windows'
  shared_lib = 'rutabaga_gfx_ffi.dll'
endif
if target_os == 'darwin'
  shared_lib = 'librutabaga_gfx_ffi.dylib'
endif
if target_os == 'linux'
  shared_lib = 'librutabaga_gfx_ffi.so'
endif

cmd = ['build.sh']
with_gfxstream = get_option('gfxstream')
with_virglrenderer = get_option('virglrenderer')
features = '--features='
if with_gfxstream
  features += 'gfxstream,'
endif
if with_virglrenderer
  features += 'virgl_renderer,'
endif

cmd += features

rutabaga_gfx_ffi_ct = custom_target(
  'rutabaga_gfx_ffi_build',
  output: shared_lib,
  input: ['src/lib.rs', 'Cargo.toml', 'build.sh'],
  command: cmd,
  env: {
    'RUTABAGA_GFX_FFI': '1',
    'PKG_CONFIG_PATH': get_option('pkg_config_path'),
    'CROSVM_USE_SYSTEM_VIRGLRENDERER': '1',
    'CARGO_TARGET_DIR': meson.build_root()
  },
  install: true,
  install_dir: get_option('libdir'),
)

rutabaga_gfx_ffi_h = files('src/include/rutabaga_gfx_ffi.h')

pkg = import('pkgconfig')
pkg.generate(
  libraries: rutabaga_gfx_ffi_ct,
  name: 'rutabaga_gfx_ffi',
  version: meson.project_version(),
  description: 'C FFI bindings to Rutabaga VGI',
)

install_headers(rutabaga_gfx_ffi_h,
                subdir: 'rutabaga_gfx')
