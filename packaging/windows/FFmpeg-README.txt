The bundled ffmpeg.exe is an unmodified GPL build redistributed from the MSYS2
project (package: mingw-w64-ucrt-x86_64-ffmpeg). HaloDaemon does not modify
ffmpeg; it invokes it as a separate process.

Its licensing is summarised in FFmpeg-LICENSE.md; the full operative texts ship
beside it as COPYING.GPLv3 and COPYING.LGPLv2.1. As required by GPLv3 §6(d), the
Corresponding Source for this build is available from:

  * FFmpeg source:      https://ffmpeg.org/download.html  (and https://git.ffmpeg.org/ffmpeg)
  * MSYS2 build recipe: https://github.com/msys2/MINGW-packages/tree/master/mingw-w64-ffmpeg

For the exact source matching this binary, use the ffmpeg version printed by
`ffmpeg.exe -version` together with the corresponding MSYS2 package revision.
