@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
cd /d C:\MyProjects\Astrix\client\vendor\rust-sdks\webrtc-sys

set SRC=C:\MyProjects\Astrix\client\target\release\build\webrtc-sys-cad2df4d29ee06a2\out\cxxbridge\sources\webrtc-sys\src\yuv_helper.rs.cc
set INC=-I "C:\MyProjects\Astrix\client\target\release\build\webrtc-sys-cad2df4d29ee06a2\out\cxxbridge\include" -I "C:\MyProjects\Astrix\client\target\release\build\webrtc-sys-cad2df4d29ee06a2\out\cxxbridge\crate" -I "./include" -I "C:\MyProjects\Astrix\client\webrtc-prebuilt/win-x64-release\include" -I "C:\MyProjects\Astrix\client\webrtc-prebuilt/win-x64-release\include\third_party/abseil-cpp/" -I "C:\MyProjects\Astrix\client\webrtc-prebuilt/win-x64-release\include\third_party/libyuv/include/" -I "C:\MyProjects\Astrix\client\webrtc-prebuilt/win-x64-release\include\third_party/libc++/" -I "C:\MyProjects\Astrix\client\webrtc-prebuilt/win-x64-release\include\sdk/objc" -I "C:\MyProjects\Astrix\client\webrtc-prebuilt/win-x64-release\include\sdk/objc/base" -I "src/mft"

cl.exe -nologo -MT -O2 -Brepro %INC% /std:c++20 /EHsc -DNOMINMAX -DWEBRTC_WIN -DUSE_MFT_VIDEO_CODEC=1 -DUSE_AURA=1 -D_HAS_EXCEPTIONS=0 -D_WINDOWS -DWIN32 -DWIN32_LEAN_AND_MEAN -D_UNICODE -DUNICODE -DNDEBUG -DWEBRTC_USE_H264 -DWEBRTC_LIBRARY_IMPL -DCHROMIUM -DLIBYUV_DISABLE_NEON -c "%SRC%" 2>&1
