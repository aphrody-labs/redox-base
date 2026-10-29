#!/bin/sh

set -e

old_path="/usr/bin/ipcd"
new_path="target/debug/ipcd"
if [ -e "${new_path}" ]
then
    mv -v "${new_path}" "${old_path}"
    shutdown --reboot
fi

while [ "$#" != "0" ]
do
    example="$1"
    shift

    echo "# ${example} #"
    "target/debug/examples/${example}"
done
