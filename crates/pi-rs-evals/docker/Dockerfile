# The Linux launcher stages native pi, pi-evals, their ELF dependencies,
# runtime tools and CA trust into this context. No registry pull is needed.
FROM scratch AS evaluator
COPY runtime/ /
ENV PATH=/usr/local/bin:/bin \
    PI_PACKAGE_DIR=/opt/pi \
    PI_EVAL_REPO_ROOT=/opt/pi \
    PI_EVAL_PI_BINARY=/usr/local/bin/pi \
    PI_CODING_AGENT_DIR=/run/pi-eval-secrets \
    PI_EVAL_CONTAINER=1 \
    HOME=/tmp
WORKDIR /tmp
ENTRYPOINT ["/usr/local/bin/pi-evals"]

FROM evaluator AS without_docs
ENV PI_EVAL_VARIANT=without_docs

FROM evaluator AS with_docs
COPY documentation/ /opt/pi/
ENV PI_EVAL_VARIANT=with_docs
