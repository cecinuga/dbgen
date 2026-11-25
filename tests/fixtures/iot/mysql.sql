-- IoT telemetry in MySQL: UUIDs as CHAR(36), JSON, a composite unique key, and readings
-- keyed by (device, instant) with millisecond timestamps.

CREATE TABLE sites (
    site_id     SMALLINT NOT NULL PRIMARY KEY,
    name        VARCHAR(80) NOT NULL,
    latitude    DOUBLE NOT NULL,
    longitude   DOUBLE NOT NULL,
    timezone    VARCHAR(40) NOT NULL,
    elevation_m INT NULL,
    CONSTRAINT ck_latitude CHECK (latitude BETWEEN -90 AND 90),
    CONSTRAINT ck_longitude CHECK (longitude BETWEEN -180 AND 180)
) ENGINE = InnoDB;

CREATE TABLE device_models (
    model_id     INT NOT NULL PRIMARY KEY,
    vendor       VARCHAR(40) NOT NULL,
    model_name   VARCHAR(40) NOT NULL,
    firmware_min VARCHAR(16) NULL,
    UNIQUE KEY uq_vendor_model (vendor, model_name)
) ENGINE = InnoDB;

CREATE TABLE devices (
    device_id     CHAR(36) NOT NULL PRIMARY KEY,
    serial_number VARCHAR(32) NOT NULL UNIQUE,
    site_id       SMALLINT NOT NULL,
    model_id      INT NOT NULL,
    installed_at  DATETIME NOT NULL,
    ip_address    VARCHAR(45) NULL,
    status        ENUM('active', 'maintenance', 'retired') NOT NULL,
    tags          JSON NULL,
    CONSTRAINT fk_devices_site FOREIGN KEY (site_id) REFERENCES sites (site_id),
    CONSTRAINT fk_devices_model FOREIGN KEY (model_id) REFERENCES device_models (model_id)
) ENGINE = InnoDB;

CREATE TABLE readings (
    device_id   CHAR(36) NOT NULL,
    measured_at DATETIME(3) NOT NULL,
    temperature FLOAT NOT NULL,
    humidity    FLOAT NOT NULL,
    battery     TINYINT UNSIGNED NULL,
    PRIMARY KEY (device_id, measured_at),
    CONSTRAINT fk_readings_device FOREIGN KEY (device_id) REFERENCES devices (device_id),
    CONSTRAINT ck_temperature CHECK (temperature BETWEEN -50 AND 150),
    CONSTRAINT ck_humidity CHECK (humidity BETWEEN 0 AND 100),
    CONSTRAINT ck_battery CHECK (battery BETWEEN 0 AND 100)
) ENGINE = InnoDB;

CREATE TABLE alerts (
    alert_id        BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    device_id       CHAR(36) NOT NULL,
    raised_at       DATETIME(3) NOT NULL,
    severity        ENUM('info', 'warning', 'critical') NOT NULL,
    code            SMALLINT NOT NULL,
    message         TEXT NULL,
    acknowledged_at DATETIME(3) NULL,
    INDEX idx_alerts_raised (raised_at),
    CONSTRAINT fk_alerts_device FOREIGN KEY (device_id) REFERENCES devices (device_id),
    CONSTRAINT ck_ack CHECK (acknowledged_at IS NULL OR acknowledged_at >= raised_at)
) ENGINE = InnoDB;
