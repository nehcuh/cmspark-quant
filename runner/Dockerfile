# Fixed, version-pinned signal-runner image. Build: scripts/build_runner.sh
# Network is only used at BUILD time; containers run with NetworkMode=none.
# The image only produces positions; all scoring happens on the trusted host.
FROM python@sha256:a6e34c598f2467ed0e9a8d349809fcd8b5c603269512df273a0bb1784edc11b1
COPY requirements.lock /runner/requirements.lock
RUN pip install --no-cache-dir -r /runner/requirements.lock
COPY signal_runner.py /runner/signal_runner.py
LABEL cmsq.runner.version="2.0.0" cmsq.runner.contract="positions-only"
USER 10001:10001
WORKDIR /tmp
ENTRYPOINT ["python", "-I", "-B", "/runner/signal_runner.py"]
