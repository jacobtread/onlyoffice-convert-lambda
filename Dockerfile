FROM amazonlinux:2023

ENV LANG=en_US.UTF-8 \
    LANGUAGE=en_US:en \
    LC_ALL=en_US.UTF-8

# Arguments
ARG ONLYOFFICE_VALUE=onlyoffice
ARG PACKAGE_VERSION=9.2.0
ARG PACKAGE_BASEURL="https://github.com/ONLYOFFICE/DocumentServer/releases/download"

# Install dependencies
RUN dnf -y update && \
    dnf -y install \
    wget \
    unzip \
    xorg-x11-server-Xvfb \
    xorg-x11-xauth \
    xorg-x11-utils \
    which \
    glibc-langpack-en \
    cairo \
    nss \
    nspr \
    libxml2 \
    libXScrnSaver \
    libXtst \
    alsa-lib \
    boost-regex \
    cabextract \
    unixODBC \
    unixODBC-devel \
    libaio \
    fontconfig \
    mkfontscale \
    mkfontdir \
    xset \
    freetype && \
    dnf clean all

# Install Microsoft TrueType fonts (Amazon Linux way)
RUN rpm -i https://downloads.sourceforge.net/project/mscorefonts2/rpms/msttcore-fonts-installer-2.6-1.noarch.rpm --nodeps && \
    fc-cache -v -r

# Copy custom fonts
COPY fonts/ /usr/share/fonts/truetype/
RUN fc-cache -f

# Download and install document server
RUN ARCH=$(uname -m) && \
    if [ "$ARCH" = "x86_64" ]; then ARCH="x86_64"; \
    elif [ "$ARCH" = "aarch64" ]; then ARCH="aarch64"; \
    else echo "Unsupported architecture: $ARCH" >&2; exit 1; fi && \
    PACKAGE_FILE="onlyoffice-documentserver.${ARCH}.rpm" && \
    wget -q -P /tmp ${PACKAGE_BASEURL}/v${PACKAGE_VERSION}/${PACKAGE_FILE} && \
    rpm -i --nodeps --nopost /tmp/$PACKAGE_FILE && \
    rm -f /tmp/$PACKAGE_FILE

# Remove unused components to reduce size
RUN rm -rf /var/www/onlyoffice/documentserver-example && \
    rm -rf /var/www/onlyoffice/documentserver/server/AdminPanel && \
    rm -rf /var/www/onlyoffice/documentserver/server/DocService && \
    rm -rf /var/www/onlyoffice/documentserver/server/FileConverter/converter && \
    rm -rf /var/www/onlyoffice/documentserver/server/info && \
    rm -rf /var/www/onlyoffice/documentserver/server/Metrics && \
    rm -rf /var/www/onlyoffice/documentserver/server/schema && \
    rm -rf /var/www/onlyoffice/documentserver/server/welcome && \
    rm -rf /var/www/onlyoffice/documentserver/web-apps/apps

# Setup script
COPY setup.sh /app/setup.sh
RUN chmod +x /app/setup.sh && /app/setup.sh

# Download lambda script
WORKDIR /app
RUN ARCH=$(uname -m) && \
    if [ "$ARCH" = "x86_64" ]; then FILE="bootstrap"; \
    elif [ "$ARCH" = "aarch64" ]; then FILE="bootstrap-arm64"; \
    else echo "Unsupported architecture: $ARCH" >&2; exit 1; fi && \
    curl -L -o bootstrap https://github.com/jacobtread/onlyoffice-convert-lambda/releases/download/0.1.1/${FILE} && \
    chmod +x /app/bootstrap

CMD ["/app/bootstrap"]
