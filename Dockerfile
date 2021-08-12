
From rust

RUN apt-get update && apt-get install -y dbus dbus-tests vim htop time
COPY bench.sh /bench.sh

ENTRYPOINT ["/bin/bash"]
