# Сборка через клон livekit/rust-sdks с применением кастомных H264 патчей.
# Требует: git, Python 3, VS 2022, Windows SDK 10 (Debugging Tools необязательны — патч применяется автоматически).

$ErrorActionPreference = "Stop"
$ClientDir = Split-Path -Parent $PSScriptRoot  # client/
$WorkDir = Join-Path $env:TEMP "astrix-webrtc-build"
$PatchesDir = Join-Path $ClientDir "vendor\libwebrtc\patches"
$LibWebrtc = Join-Path $WorkDir "webrtc-sys\libwebrtc"

Write-Host "Cloning livekit/rust-sdks..." -ForegroundColor Cyan
if (Test-Path $WorkDir) {
    $maxRetries = 3
    for ($i = 1; $i -le $maxRetries; $i++) {
        try {
            Remove-Item -Recurse -Force $WorkDir -ErrorAction Stop
            break
        } catch {
            if ($i -eq $maxRetries) {
                Write-Host "Cannot remove $WorkDir - files in use." -ForegroundColor Red
                Write-Host "Close depot_tools/python processes, or delete the folder manually, then retry." -ForegroundColor Yellow
                exit 1
            }
            Write-Host "Retry $i/$maxRetries in 5 sec..." -ForegroundColor Yellow
            Start-Sleep -Seconds 5
        }
    }
}
git clone --depth 1 https://github.com/livekit/rust-sdks.git $WorkDir

# --- Python-скрипты для патчинга H264 исходников ---
# Используем прямую замену строк вместо git apply: надёжнее при смене версий WebRTC.

$h264MultithreadPatch = @'
import os, sys

f = os.path.join('src', 'modules', 'video_coding', 'codecs', 'h264', 'h264_encoder_impl.cc')
if not os.path.exists(f):
    print(f'[skip] {f} not found'); sys.exit(0)

with open(f, 'r', encoding='utf-8') as fp: c = fp.read()

if '#if defined(WEBRTC_WIN)' in c and 'number_of_cores / 2' in c:
    print('h264_multithread already patched'); sys.exit(0)

# Вставить Windows-блок перед TODO-комментарием после VGA return
marker = '  // TODO(sprang): Also check sSliceArgument.uiSliceNum on GetEncoderParams(),'
if marker not in c:
    print(f'[warn] marker not found in {f}'); sys.exit(1)

insert = (
    '  // On Windows, sandbox does not apply (crbug.com/583348 is Mac-specific).\n'
    '  // Use physical cores (OpenH264 scales poorly with logical/HT cores).\n'
    '  // Heuristic: even cores >= 4 likely means hyperthreading (e.g. 8c/16t -> 8).\n'
    '#if defined(WEBRTC_WIN)\n'
    '  int cores = (number_of_cores >= 4 && (number_of_cores % 2) == 0)\n'
    '                  ? (number_of_cores / 2)\n'
    '                  : number_of_cores;\n'
    '  if (width * height >= 1920 * 1080 && cores > 8) {\n'
    '    return 8;\n'
    '  } else if (width * height >= 1920 * 1080 && cores >= 6) {\n'
    '    return 6;\n'
    '  } else if (width * height > 1280 * 960 && cores >= 4) {\n'
    '    return 4;\n'
    '  } else if (width * height > 640 * 480 && cores >= 3) {\n'
    '    return 2;\n'
    '  }\n'
    '#endif\n'
    '  // TODO(sprang): Also check sSliceArgument.uiSliceNum on GetEncoderParams(),'
)
c = c.replace(marker, insert, 1)
with open(f, 'w', encoding='utf-8') as fp: fp.write(c)
print('Applied h264_multithread patch')
'@

$h264DecoderPatch = @'
import os, sys, re

f = os.path.join('src', 'modules', 'video_coding', 'codecs', 'h264', 'h264_decoder_impl.cc')
if not os.path.exists(f):
    print(f'[skip] {f} not found'); sys.exit(0)

with open(f, 'r', encoding='utf-8') as fp: c = fp.read()

if '#if defined(WEBRTC_WIN)' in c and 'thread_count = thread_count' in c:
    print('h264_decoder_multithread already patched'); sys.exit(0)

# Заменяем блок: комментарий + av_context_->thread_count = 1
old = re.search(
    r'  // If this is ever increased.*?thread checker in the frame buffer pool\.\n'
    r'  av_context_->thread_count = 1;',
    c, re.DOTALL
)
if not old:
    print(f'[warn] decoder thread_count pattern not found'); sys.exit(1)

replacement = (
    '  // Multithreaded decoding for viewer (Windows). FFmpeg slice threading.\n'
    '  // If this is ever increased, look at `av_context_->thread_safe_callbacks` and\n'
    '  // make it possible to disable the thread checker in the frame buffer pool.\n'
    '#if defined(WEBRTC_WIN)\n'
    '  int thread_count = 1;\n'
    '  if (resolution.Valid()) {\n'
    '    int pixels = resolution.Width() * resolution.Height();\n'
    '    int cores = settings.number_of_cores();\n'
    '    if (cores < 1) cores = 1;\n'
    '    if (pixels >= 1920 * 1080 && cores >= 6) {\n'
    '      thread_count = 4;\n'
    '    } else if (pixels > 1280 * 720 && cores >= 4) {\n'
    '      thread_count = 2;\n'
    '    }\n'
    '  }\n'
    '  av_context_->thread_count = thread_count;\n'
    '#else\n'
    '  av_context_->thread_count = 1;\n'
    '#endif'
)
c = c[:old.start()] + replacement + c[old.end():]
with open(f, 'w', encoding='utf-8') as fp: fp.write(c)
print('Applied h264_decoder_multithread patch')
'@

$h264EncoderParamsPatch = @'
import os, sys

f = os.path.join('src', 'modules', 'video_coding', 'codecs', 'h264', 'h264_encoder_impl.cc')
if not os.path.exists(f):
    print(f'[skip] {f} not found'); sys.exit(0)

with open(f, 'r', encoding='utf-8') as fp: c = fp.read()

changed = False

# 1. bEnableFrameSkip: force true
old = '  encoder_params.bEnableFrameSkip = configurations_[i].frame_dropping_on;'
new = ('  // Astrix: force frame skip for realtime (stabilizes FPS when bitrate exceeded).\n'
       '  encoder_params.bEnableFrameSkip = true;')
if old in c:
    c = c.replace(old, new, 1); changed = True
    print('  bEnableFrameSkip patched')
else:
    print('  [skip] bEnableFrameSkip pattern not found')

# 2. uiIntraPeriod: 2-second keyframe
old = '  encoder_params.uiIntraPeriod = configurations_[i].key_frame_interval;'
new = ('  // Astrix: 2-second keyframe interval for screen share (reduces IDR stalls).\n'
       '  encoder_params.uiIntraPeriod = static_cast<unsigned int>(\n'
       '      configurations_[i].max_frame_rate * 2);')
if old in c:
    c = c.replace(old, new, 1); changed = True
    print('  uiIntraPeriod patched')
else:
    print('  [skip] uiIntraPeriod pattern not found')

# 3. uiMaxNalSize + add iNumRefFrame
old = '  encoder_params.uiMaxNalSize = 0;'
new = ('  // Astrix: limit NAL size to 1200 bytes to reduce keyframe decode stalls.\n'
       '  encoder_params.uiMaxNalSize = 1200;\n'
       '  // Astrix: single reference frame for low latency and reduced decode stalls.\n'
       '  encoder_params.iNumRefFrame = 1;')
if old in c:
    c = c.replace(old, new, 1); changed = True
    print('  uiMaxNalSize patched')
else:
    print('  [skip] uiMaxNalSize pattern not found')

# 4. NonInterleaved: multi-slice
old = (
    '      // When uiSliceMode = SM_FIXEDSLCNUM_SLICE, uiSliceNum = 0 means auto\n'
    '      // design it with cpu core number.\n'
    '      // TODO(sprang): Set to 0 when we understand why the rate controller borks\n'
    '      //               when uiSliceNum > 1.\n'
    '      encoder_params.sSpatialLayers[0].sSliceArgument.uiSliceNum = 1;'
)
new = (
    '      // Astrix: multi-slice for parallel encode/decode (720p->2-3, 1080p->4, 1440p->6).\n'
    '      // Capped by iMultipleThreadIdc. bEnableAdaptiveQuant=false workaround for\n'
    '      // OpenH264 rate controller bug with uiSliceNum>1 (cisco/openh264#2591).\n'
    '      {\n'
    '        int pixels = encoder_params.iPicWidth * encoder_params.iPicHeight;\n'
    '        int max_threads = encoder_params.iMultipleThreadIdc;\n'
    '        int slice_num = 1;\n'
    '        float fps = configurations_[i].max_frame_rate;\n'
    '        if (pixels >= 2560 * 1440) {\n'
    '          slice_num = (fps >= 60.0f) ? 6 : 4;\n'
    '        } else if (pixels >= 1920 * 1080) {\n'
    '          slice_num = (fps >= 120.0f) ? 6 : 4;\n'
    '        } else if (pixels > 1280 * 720) {\n'
    '          slice_num = (fps >= 60.0f) ? 4 : 3;\n'
    '        } else if (pixels > 640 * 480) {\n'
    '          if (fps >= 120.0f) slice_num = 4;\n'
    '          else if (fps >= 60.0f) slice_num = 3;\n'
    '          else slice_num = 2;\n'
    '        }\n'
    '        if (slice_num > max_threads) slice_num = max_threads;\n'
    '        encoder_params.sSpatialLayers[0].sSliceArgument.uiSliceNum = slice_num;\n'
    '        if (slice_num > 1) encoder_params.bEnableAdaptiveQuant = false;\n'
    '      }'
)
if old in c:
    c = c.replace(old, new, 1); changed = True
    print('  NonInterleaved multi-slice patched')
else:
    print('  [skip] NonInterleaved pattern not found')

if changed:
    with open(f, 'w', encoding='utf-8') as fp: fp.write(c)
    print('Applied h264_encoder_params patch')
else:
    print('h264_encoder_params: nothing to patch (already applied or patterns changed)')
'@

# Записать Python-скрипты патчей в рабочую директорию
[System.IO.File]::WriteAllText((Join-Path $LibWebrtc "patch_h264_multithread.py"),   $h264MultithreadPatch,   [System.Text.Encoding]::UTF8)
[System.IO.File]::WriteAllText((Join-Path $LibWebrtc "patch_h264_decoder.py"),       $h264DecoderPatch,       [System.Text.Encoding]::UTF8)
[System.IO.File]::WriteAllText((Join-Path $LibWebrtc "patch_h264_encoder_params.py"),$h264EncoderParamsPatch, [System.Text.Encoding]::UTF8)

# Скрипт патча vs_toolchain.py: делает dbghelp.dll необязательным.
$toolchainPatch = @'
import os, sys
f = os.path.join('src', 'build', 'vs_toolchain.py')
if not os.path.exists(f):
    print('vs_toolchain.py not found, skipping patch')
    sys.exit(0)
with open(f, 'r', encoding='utf-8') as fp:
    c = fp.read()
if "('dbghelp.dll', False)" in c:
    c = c.replace("('dbghelp.dll', False)", "('dbghelp.dll', True)")
    with open(f, 'w', encoding='utf-8') as fp:
        fp.write(c)
    print('Patched vs_toolchain.py: dbghelp.dll is now optional')
else:
    print('vs_toolchain.py already patched or structure changed')
'@
[System.IO.File]::WriteAllText((Join-Path $LibWebrtc "patch_toolchain.py"), $toolchainPatch, [System.Text.Encoding]::UTF8)

# Добавить вызовы Python-патчей и toolchain-фикс в build_windows.cmd
# (H264 патчи запускаются после gclient sync + git apply исходных livekit патчей)
$BuildCmd = Join-Path $LibWebrtc "build_windows.cmd"
$Content = Get-Content $BuildCmd -Raw -Encoding UTF8
# ВАЖНО: python3 резолвится в depot_tools\python3.bat.
# В cmd вызов .bat без "call" завершает родительский скрипт после возврата.
# Поэтому все вызовы python3 должны идти через "call python3".
if ($Content -notmatch "patch_h264_multithread.py") {
    $Insert = (
        "call python3 `"%COMMAND_DIR%/patch_h264_multithread.py`"`r`n" +
        "call python3 `"%COMMAND_DIR%/patch_h264_decoder.py`"`r`n" +
        "call python3 `"%COMMAND_DIR%/patch_h264_encoder_params.py`"`r`n"
    )
    $Content = $Content -replace '(rem generate ninja for release\r?\n)', "${Insert}`$1"
    [System.IO.File]::WriteAllText($BuildCmd, $Content)
}
# Вставить toolchain-патч тоже (если ещё нет)
$Content = Get-Content $BuildCmd -Raw -Encoding UTF8
if ($Content -notmatch "patch_toolchain.py") {
    $Insert = "call python3 `"%COMMAND_DIR%/patch_toolchain.py`"`r`n"
    $Content = $Content -replace '(rem generate ninja for release\r?\n)', "${Insert}`$1"
    [System.IO.File]::WriteAllText($BuildCmd, $Content)
}
# ninja.exe находится в src\third_party\ninja\ (туда кладёт gclient),
# но build_windows.cmd его туда не добавляет в PATH — фиксим
$Content = Get-Content $BuildCmd -Raw -Encoding UTF8
if ($Content -notmatch "third_party\\\\ninja") {
    $Insert = "set PATH=%cd%\src\third_party\ninja;%PATH%`r`n"
    $Content = $Content -replace '(rem build\r?\n)', "${Insert}`$1"
    [System.IO.File]::WriteAllText($BuildCmd, $Content)
}

Write-Host "Running build_windows.cmd (30-60 min)..." -ForegroundColor Yellow
Push-Location $LibWebrtc
try {
    cmd /c "build_windows.cmd --arch x64 --profile release"
    # build_windows.cmd не всегда пробрасывает коды ошибок корректно,
    # поэтому проверяем артефакты вручную ниже.

    $BuildOut  = Join-Path $LibWebrtc "src\out-x64-release"
    $Artifacts = Join-Path $LibWebrtc "win-x64-release"
    $OutPrebuilt = Join-Path $ClientDir "webrtc-prebuilt\win-x64-release"

    # --- Проверка webrtc.lib ---
    $LibDest = Join-Path $Artifacts "lib\webrtc.lib"
    if (-not (Test-Path $LibDest)) {
        $LibSrc = Join-Path $BuildOut "obj\webrtc.lib"
        if (Test-Path $LibSrc) {
            New-Item -ItemType Directory -Path (Join-Path $Artifacts "lib") -Force | Out-Null
            Copy-Item $LibSrc $LibDest -Force
            Write-Host "Copied webrtc.lib from build output." -ForegroundColor Yellow
        } else {
            throw "Build failed: webrtc.lib not found.`nExpected: $LibDest`nAlso checked: $LibSrc`nCheck build output above for ninja/compiler errors."
        }
    } else {
        Write-Host "webrtc.lib OK ($([math]::Round((Get-Item $LibDest).Length / 1MB, 1)) MB)" -ForegroundColor Green
    }

    # --- Проверка ninja файлов в артефактах; копируем из build output если нет ---
    $ninjaMap = @{
        "webrtc.ninja"           = "obj\webrtc.ninja"
        "desktop_capture.ninja"  = "obj\modules\desktop_capture\desktop_capture.ninja"
    }
    foreach ($leaf in $ninjaMap.Keys) {
        $dst = Join-Path $Artifacts $leaf
        if (-not (Test-Path $dst)) {
            $src = Join-Path $BuildOut $ninjaMap[$leaf]
            if (Test-Path $src) {
                Copy-Item $src $dst -Force
                Write-Host "Copied $leaf from build output." -ForegroundColor Yellow
            }
        }
    }

    # --- Копируем артефакты в webrtc-prebuilt ---
    if (Test-Path (Join-Path $ClientDir "webrtc-prebuilt")) {
        Remove-Item -Recurse -Force (Join-Path $ClientDir "webrtc-prebuilt")
    }
    New-Item -ItemType Directory -Path (Split-Path $OutPrebuilt) -Force | Out-Null
    Copy-Item $Artifacts $OutPrebuilt -Recurse
    Write-Host "Artifacts copied to $OutPrebuilt" -ForegroundColor Cyan

    # --- Если ninja файлы всё ещё отсутствуют — скачать из LiveKit prebuilt ---
    $missingNinja = @()
    foreach ($leaf in $ninjaMap.Keys) {
        if (-not (Test-Path (Join-Path $OutPrebuilt $leaf))) { $missingNinja += $leaf }
    }

    if ($missingNinja.Count -gt 0) {
        Write-Host "Ninja files missing after build. Downloading from LiveKit prebuilt..." -ForegroundColor Yellow
        $zipPath = Join-Path $env:TEMP "webrtc-ninja-fix.zip"
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $reqHeaders = @{ "User-Agent" = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) PowerShell" }
        $downloaded = $false
        foreach ($tag in @("webrtc-0001d84-2", "webrtc-38b585d")) {
            $url = "https://github.com/livekit/rust-sdks/releases/download/$tag/webrtc-win-x64-release.zip"
            try {
                Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing -Headers $reqHeaders -MaximumRedirection 5 -ErrorAction Stop
                if ((Get-Item $zipPath).Length -gt 1000000) {
                    $downloaded = $true
                    Write-Host "  Downloaded from $tag" -ForegroundColor Green
                    break
                }
            } catch {
                Write-Host "  $tag : $($_.Exception.Message)" -ForegroundColor Yellow
            }
        }

        if ($downloaded) {
            $extractDir = Join-Path $env:TEMP "webrtc-ninja-extract"
            if (Test-Path $extractDir) { Remove-Item -Recurse -Force $extractDir }
            Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
            $found = $null
            foreach ($d in @(
                (Join-Path $extractDir "webrtc-win-x64-release"),
                (Join-Path $extractDir "win-x64-release"),
                $extractDir
            )) {
                if (Test-Path (Join-Path $d "webrtc.ninja")) { $found = $d; break }
            }
            if ($found) {
                foreach ($leaf in $missingNinja) {
                    $src = Join-Path $found $leaf
                    if (Test-Path $src) {
                        Copy-Item $src (Join-Path $OutPrebuilt $leaf) -Force
                        Write-Host "  Installed $leaf from LiveKit prebuilt." -ForegroundColor Green
                    }
                }
            }
            Remove-Item $zipPath -Force -ErrorAction SilentlyContinue
            Remove-Item $extractDir -Recurse -Force -ErrorAction SilentlyContinue
        }

        # Последний резерв: минимальные ninja файлы с нужными -D флагами
        $defines = " -D NDEBUG -D NOMINMAX -D WIN32 -D _WIN32 -D WEBRTC_WIN -D RTC_USE_H264=1"
        foreach ($leaf in $missingNinja) {
            if (-not (Test-Path (Join-Path $OutPrebuilt $leaf))) {
                Set-Content (Join-Path $OutPrebuilt $leaf) $defines -Encoding ASCII
                Write-Warning "Created minimal $leaf (download failed). Preprocessor defines may be incomplete."
            }
        }
    }

    # --- Финальная проверка ---
    $finalLib = Join-Path $OutPrebuilt "lib\webrtc.lib"
    if (-not (Test-Path $finalLib)) {
        Write-Warning "lib\webrtc.lib missing in output! cargo build will fail."
    } else {
        Write-Host "lib\webrtc.lib OK" -ForegroundColor Green
    }
    foreach ($leaf in $ninjaMap.Keys) {
        if (Test-Path (Join-Path $OutPrebuilt $leaf)) {
            Write-Host "$leaf OK" -ForegroundColor Green
        } else {
            Write-Warning "$leaf still missing."
        }
    }

    Write-Host "Done. Uncomment LK_CUSTOM_WEBRTC in .cargo/config.toml" -ForegroundColor Green
} finally {
    Pop-Location
}
