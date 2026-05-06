# IndirectDisplay assessment

Microsoft `video/IndirectDisplay` uses IddCx to create an indirect display device and a software monitor. The sample driver receives swap-chain work for that indirect monitor through its own device context.

For this R&D question, IndirectDisplay is useful as a control case:

- It can prove that a driver sees frames for the display device it owns.
- It does not prove visibility into a physical NVIDIA/AMD/Intel monitor.
- It should not receive physical output scan-out surfaces from another vendor miniport.

Expected classification:

- **B** if the sample receives frames only for its virtual/indirect monitor.
- **C** for physical monitor capture, because no physical output data is exposed.

Relevant sample files:

- `upstream/Windows-driver-samples/video/IndirectDisplay/IddSampleDriver/Driver.cpp`
- `upstream/Windows-driver-samples/video/IndirectDisplay/IddSampleDriver/Driver.h`
- `upstream/Windows-driver-samples/video/IndirectDisplay/IddSampleDriver/Trace.h`

If this path is explored further, add logging around:

- device D0 entry/exit;
- adapter initialization;
- monitor arrival/departure;
- swap-chain arrival/departure;
- swap-chain frame processing loop.

Do not treat the virtual monitor swap-chain as evidence that physical display capture is possible.
