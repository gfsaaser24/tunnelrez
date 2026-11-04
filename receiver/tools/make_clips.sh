#!/bin/bash
# Generate the NVENC test clips used by send_test.py into tools/clips/ (about 1 GB in total).
set -e
cd "$(dirname "$0")" && mkdir -p clips && cd clips
LL="-preset p1 -tune ull -bf 0 -g 600 -rc cbr -zerolatency 1"
SRC1080="testsrc2=size=1920x1080:rate=60"
SRC4K="testsrc2=size=3840x2160:rate=60"
GRID="drawgrid=w=8:h=8:t=1:c=white@1,drawbox=x=1600:y=900:w=640:h=360:c=black:t=fill,drawgrid=x=1600:y=900:w=2:h=2:t=1:c=white"
q="-hide_banner -loglevel error -y -f lavfi"
# 1080p60, 30 Mb/s
ffmpeg $q -i $SRC1080 -t 30 -c:v h264_nvenc $LL -b:v 30M -bufsize 500k -profile:v high -bsf:v h264_mp4toannexb -f h264 test.h264
ffmpeg $q -i $SRC1080 -t 30 -c:v hevc_nvenc $LL -b:v 30M -bufsize 500k -bsf:v hevc_mp4toannexb -f hevc test.hevc
ffmpeg $q -i $SRC1080 -t 30 -c:v av1_nvenc  -preset p1 -tune ull -bf 0 -g 600 -rc cbr -b:v 30M -bufsize 500k -f obu test.obu
# 2160p60, 55 Mb/s, with 1-px grid areas
ffmpeg $q -i $SRC4K -vf "$GRID" -t 30 -c:v hevc_nvenc $LL -b:v 55M -bufsize 1M -bsf:v hevc_mp4toannexb -f hevc test4k.hevc
ffmpeg $q -i $SRC4K -vf "$GRID" -t 30 -c:v av1_nvenc -preset p1 -tune ull -bf 0 -g 600 -rc cbr -b:v 55M -bufsize 1M -f obu test4k.obu
ffmpeg $q -i $SRC4K -vf "$GRID" -t 30 -c:v h264_nvenc $LL -b:v 55M -bufsize 1M -profile:v high -level 5.2 -bsf:v h264_mp4toannexb -f h264 test4k.h264
# Static 4K sharpness pattern: left = 1-px dot checker, right = 1-px horizontal lines, moving square in a blue box.
ffmpeg $q -i "color=c=black:s=3840x2160:r=60" -vf "drawgrid=x=0:y=0:w=2:h=4000:t=1:c=white,drawbox=x=1920:y=0:w=1920:h=2160:c=black:t=fill,drawgrid=x=1920:y=0:w=4000:h=2:t=1:c=white,drawbox=x=1280:y=880:w=1280:h=400:c=0x204080:t=fill,drawbox=x='1280+mod(t*400\,1200)':y=1040:w=80:h=80:c=white:t=fill" \
  -t 25 -c:v hevc_nvenc -preset p4 -tune ull -bf 0 -g 600 -rc cbr -b:v 60M -bufsize 2M -zerolatency 1 -bsf:v hevc_mp4toannexb -f hevc sharp4k.hevc
ls -la
