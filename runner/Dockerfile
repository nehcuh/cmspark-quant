# Fixed, version-pinned signal-runner image. Build: scripts/build_runner.sh
# Network is only used at BUILD time; containers run with NetworkMode=none.
# The image only produces positions; all scoring happens on the trusted host.
FROM python@sha256:a6e34c598f2467ed0e9a8d349809fcd8b5c603269512df273a0bb1784edc11b1
# libfaketime: the container wall clock is frozen at 2000-01-01 (defence in depth:
# containers also only ever see fake bar dates, never real ones)
RUN apt-get update && apt-get install -y --no-install-recommends libfaketime && rm -rf /var/lib/apt/lists/*
COPY requirements.lock /runner/requirements.lock
RUN pip install --no-cache-dir -r /runner/requirements.lock
COPY signal_runner.py vnpy_shim.py /runner/
LABEL cmsq.runner.version="2.2.1" cmsq.runner.contract="positions-only"
ENV LD_PRELOAD=/usr/lib/x86_64-linux-gnu/faketime/libfaketime.so.1 FAKETIME="2000-01-01 00:00:00" FAKETIME_DONT_RESET=1 TZ=UTC
USER 10001:10001
WORKDIR /tmp
ENTRYPOINT ["python", "-I", "-B", "/runner/signal_runner.py"]
