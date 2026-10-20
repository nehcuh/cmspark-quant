# Fixed, version-pinned backtest runner image. Build: scripts/build_runner.sh
# Network is only used at BUILD time; containers run with NetworkMode=none.
FROM python:3.12-slim
COPY requirements.lock /runner/requirements.lock
RUN pip install --no-cache-dir -r /runner/requirements.lock \
 && useradd -u 10001 -M -s /usr/sbin/nologin runner
COPY run_backtest.py /runner/run_backtest.py
USER 10001:10001
WORKDIR /tmp
ENTRYPOINT ["python", "-I", "-B", "/runner/run_backtest.py"]
