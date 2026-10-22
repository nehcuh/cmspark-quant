# Kronos variant of the signal runner (UNTRUSTED side). Same contract, same
# sandbox, same frozen clock: positions only. torch lives ONLY in this image,
# never in the trusted host process. Build: scripts/build_runner_kronos.sh
FROM cmsq-runner:2.2.3
USER root
ENV LD_PRELOAD=
COPY requirements.lock /runner/kronos/requirements.lock
RUN pip install --no-cache-dir -r /runner/kronos/requirements.lock
COPY kronos_code /runner/kronos/kronos_code
COPY weights /runner/kronos/weights
COPY SHA256SUMS kronos.lock cmsq_kronos.py gen_synthetic.py /runner/kronos/
COPY signal_runner.py /runner/signal_runner.py
# fail the build if any pinned artifact differs
RUN cd /runner/kronos && sha256sum -c --quiet SHA256SUMS && chmod -R a-w /runner/kronos
ARG MANIFEST_SHA256
LABEL cmsq.runner.kronos.manifest="${MANIFEST_SHA256}" cmsq.runner.kronos.license="MIT (c) 2025 ShiYu, see /runner/kronos/kronos_code/LICENSE"
ENV LD_PRELOAD=/usr/lib/x86_64-linux-gnu/faketime/libfaketime.so.1 \
    PYTHONPATH=/runner/kronos:/runner/kronos/kronos_code \
    HF_HUB_OFFLINE=1 OMP_NUM_THREADS=1 MKL_NUM_THREADS=1 CMSQ_DETERMINISTIC_TORCH=1
USER 10001:10001
