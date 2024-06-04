#!/bin/bash

cargo build $@
rm librutabaga_gfx_ffi.so 2>/dev/null
cp debug/librutabaga_gfx_ffi.so ./
