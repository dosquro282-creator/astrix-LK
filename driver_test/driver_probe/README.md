# driver_probe

R&D-проект для проверки, можно ли на Windows увидеть кадры, surface/allocation addresses или полезные display-driver-level callbacks физического монитора NVIDIA/AMD/Intel через display driver/WDDM DDI.

Проект намеренно не реализует захват. Он только логирует то, что безопасно предоставляет WDDM для драйвера, который реально владеет своим display-only device.

## База

Используется официальный Microsoft репозиторий [Windows-driver-samples](https://github.com/microsoft/Windows-driver-samples):

- `video/KMDOD` - основная проверка display-only miniport.
- `video/IndirectDisplay` - альтернативная модель через IddCx, см. отдельный раздел ниже.
- Зафиксированный commit: `8e9b1b1e0f91c8084dd062314896d8ea0facd608`.

Локальные пути:

- KMDOD: `upstream/Windows-driver-samples/video/KMDOD/KMDOD.sln`
- IndirectDisplay: `upstream/Windows-driver-samples/video/IndirectDisplay/IddSampleDriver.sln`

## Текущий честный вывод

Предварительный вывод по архитектуре WDDM: наиболее вероятный результат - **B** или **C**, не **A**.

- **B**: драйвер видит callbacks только для своего KMDOD/display-only device.
- **C**: на машине с обычным NVIDIA/AMD/Intel output ничего полезного для физического output не видно, потому что KMDOD не является драйвером этого адаптера.
- **A** возможен только если исследуемый драйвер находится в стеке того display device, который реально обслуживает физический output. KMDOD не является пассивным наблюдателем чужого WDDM miniport и не должен получать callbacks NVIDIA/AMD/Intel.

IndirectDisplay тоже не дает доступ к физическому output: он создает indirect/virtual monitor, получает swap-chain для этого виртуального монитора и обрабатывает только свои кадры.

## Что добавлено

В KMDOD добавлено безопасное `DbgPrintEx`-логирование через отдельные макросы `BDD_PROBE*`.

Измененные файлы:

- `upstream/Windows-driver-samples/video/KMDOD/bdd_errorlog.hxx`
- `upstream/Windows-driver-samples/video/KMDOD/bdd_ddi.cxx`
- `upstream/Windows-driver-samples/video/KMDOD/bdd.cxx`
- `upstream/Windows-driver-samples/video/KMDOD/bdd_dmm.cxx`
- `upstream/Windows-driver-samples/video/KMDOD/bdd_util.cxx`

Логируются:

- `DriverEntry`
- `AddDevice`
- `StartDevice`
- `StopDevice`
- `QueryAdapterInfo`
- `IsSupportedVidPn`
- `RecommendFunctionalVidPn`
- `CommitVidPn`
- `SetVidPnSourceVisibility`
- `UpdateActiveVidPnPresentPath`
- `PresentDisplayOnly`
- interrupt/DPC callbacks
- `MapFrameBuffer`/`UnmapFrameBuffer`

Отдельно зафиксировано, что KMDOD не регистрирует `DxgkDdiSetVidPnSourceAddress` и `DxgkDdiSetVidPnSourceAddressWithMultiPlaneOverlay`. Для display-only path основной frame callback здесь - `PresentDisplayOnly`.

## Stage 1: сборка

На тестовой машине нужны:

- Visual Studio с C++ workload.
- Windows Driver Kit с kernel-mode toolset `WindowsKernelModeDriver10.0`.
- SDK/WDK одной совместимой версии.

Команда:

```powershell
cd "C:\MyProjects\Astrix LK\astrix-LK\driver_test\driver_probe\upstream\Windows-driver-samples\video\KMDOD"
& "C:\Program Files\Microsoft Visual Studio\2022\Professional\MSBuild\Current\Bin\MSBuild.exe" KMDOD.sln /p:Configuration=Debug /p:Platform=x64 /m
```

На этой машине была найдена Visual Studio BuildTools/MSBuild, но локальная проверка сборки завершилась ожидаемо:

```text
error MSB8020: Не удается найти средства сборки для WindowsKernelModeDriver10.0
```

То есть здесь есть SDK `10.0.26100.0`, но нет WDK kernel-mode build tools.

## Stage 1: test signing и установка

Делать только на VM/test machine со snapshot/restore plan. Display miniport может оставить машину без картинки или вызвать bugcheck, особенно если ставить его поверх основного адаптера.

Включить test signing из elevated PowerShell/CMD:

```powershell
bcdedit /set testsigning on
shutdown /r /t 0
```

После перезагрузки проверить:

```powershell
bcdedit /enum {current}
```

Установить test certificate из output package, если Visual Studio/WDK сгенерировали `.cer`:

```powershell
certutil.exe -addstore Root .\SampleDisplay.cer
```

Установка INF из package/debug output:

```powershell
pnputil /add-driver .\Sample\sampledisplay.inf /install
```

Для ручного варианта через Device Manager использовать `SampleDisplay.inf` из build/package output. Для ACPI-based GPU Microsoft README предлагает добавить generic ACPI ids в INF, но это надо делать только на disposable VM/test host.

## Сбор логов

`DbgPrintEx` использует `DPFLTR_IHVVIDEO_ID`. В kernel debugger:

```text
ed nt!Kd_IHVVIDEO_Mask 0xF
ed nt!Kd_DEFAULT_MASK 0xF
```

После этого искать строки:

```text
[driver_probe][KMDOD]
```

Можно использовать WinDbg/KD или DebugView/DbgView с включенным kernel capture, если политика тестовой машины это разрешает.

## Как классифицировать A/B/C

**A** - если при активном физическом NVIDIA/AMD/Intel monitor KMDOD получает callbacks, которые явно относятся к этому physical output/device, и видит реальные frame/surface/allocation addresses этого адаптера.

**B** - если callbacks приходят только для `SampleDisplay`/KMDOD device: свои VidPN source/target ids, свой POST/display-only framebuffer, `PresentDisplayOnly` с CPU-readable source pointer от OS, без чужих allocation handles.

**C** - если KMDOD не стартует как active display device или не получает полезных frame callbacks для физического output.

Ожидаемый результат для обычной NVIDIA/AMD/Intel машины: **B/C**.

## Примеры логов

См. `examples/kmdod_expected_b.log` и `examples/local_build_attempt.log`.

Ключевые признаки B:

- `DriverEntry registering display-only DDIs; SetVidPnSourceAddress/MPO callbacks are not registered by KMDOD`
- `PresentDisplayOnly source pointer is CPU-readable by display-only DDI contract; it is not another vendor adapter VRAM pointer`
- `belongsToThisDriver=1` только для KMDOD framebuffer

## IndirectDisplay как альтернатива

IndirectDisplay/IddCx полезен для virtual display scenarios. Он может получать frames для виртуального indirect monitor через swap-chain processing, но это не capture физического HDMI/DP/eDP output NVIDIA/AMD/Intel.

Практический вывод: IndirectDisplay подходит для проверки сценария **B** - драйвер видит только то, что рендерится в его виртуальный монитор.

## Следующие шаги, если A вдруг подтвердится

Если на тестовой машине реально появляется A, следующие шаги:

- Зафиксировать точный hardware id, adapter stack, monitor path и какие callbacks пришли.
- Добавить WPP/ETW session вместо `DbgPrintEx` для частых callbacks.
- Логировать allocation identity только через официальные DDI-поля, без dereference чужих адресов.
- Проверить, является ли драйвер реально владельцем данного output или он подменил/заменил vendor miniport.
- Сверить поведение на NVIDIA, AMD, Intel отдельно.

Если подтверждается B/C, этот путь упирается в virtual/display-only device boundary. Для физического output нужен драйвер в стеке соответствующего адаптера или официальный user-mode capture API, но DXGI Desktop Duplication/WGC здесь намеренно исключены.

## Безопасность

В этом проекте не делается:

- чтение чужой VRAM напрямую;
- hooks в игры или процессы;
- DXGI Desktop Duplication/WGC;
- модификация NVIDIA/AMD/Intel driver;
- запись в чужие kernel structures;
- dereference неизвестных allocation pointers.

Логируются только поля, уже переданные KMDOD/IddCx его собственными официальными callbacks.
