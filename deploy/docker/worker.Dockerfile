# Kernos reasoning worker. Installs the SDK from this checkout with the
# anthropic extra; a worker holds no durable state, so the image has no volume.
FROM python:3.12-slim
RUN useradd --system --uid 10001 kernos
WORKDIR /opt/kernos
COPY sdk/python ./sdk/python
RUN pip install --no-cache-dir "./sdk/python[anthropic]"
USER kernos
ENV KERNOS_KERNEL_URL=http://kernel:7401 \
    KERNOS_GATEWAY_URL=http://gateway:7402 \
    KERNOS_PROVIDER=anthropic
ENTRYPOINT ["kernos-worker"]
