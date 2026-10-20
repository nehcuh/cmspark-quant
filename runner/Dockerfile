# Fixed, version-pinned backtest runner image. Build: scripts/build_runner.sh
# Network is only used at BUILD time; containers run with NetworkMode=none.
FROM python@sha256:a6e34c598f2467ed0e9a8d349809fcd8b5c603269512df273a0bb1784edc11b1
COPY requirements.lock /runner/requirements.lock
RUN pip install --no-cache-dir -r /runner/requirements.lock \
 && useradd -u 10001 -M -s /usr/sbin/nologin runner
COPY run_backtest.py /runner/run_backtest.py
USER 10001:10001
WORKDIR /tmp
ENTRYPOINT ["python", "-I", "-B", "/runner/run_backtest.py"]
