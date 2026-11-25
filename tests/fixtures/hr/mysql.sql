-- Human resources in MySQL: two self-referencing hierarchies (departments and managers),
-- a CHECK relating two columns, a history keyed by employee and date, and a generated
-- column.

CREATE TABLE departments (
    department_id        SMALLINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    name                 VARCHAR(80) NOT NULL,
    cost_center          VARCHAR(10) NOT NULL,
    parent_department_id SMALLINT NULL,
    UNIQUE KEY uk_departments_name (name),
    UNIQUE KEY uk_departments_cost_center (cost_center),
    CONSTRAINT fk_departments_parent FOREIGN KEY (parent_department_id) REFERENCES departments (department_id)
) ENGINE = InnoDB;

CREATE TABLE job_titles (
    job_title_id INT NOT NULL PRIMARY KEY,
    title        VARCHAR(80) NOT NULL,
    min_salary   DECIMAL(10, 2) NOT NULL,
    max_salary   DECIMAL(10, 2) NOT NULL,
    CONSTRAINT ck_salary_band CHECK (min_salary > 0 AND max_salary > min_salary)
) ENGINE = InnoDB;

CREATE TABLE employees (
    employee_id   INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    email         VARCHAR(120) NOT NULL,
    first_name    VARCHAR(60) NOT NULL,
    last_name     VARCHAR(60) NOT NULL,
    birth_date    DATE NOT NULL,
    hired_on      DATE NOT NULL,
    left_on       DATE NULL,
    kind          ENUM('full_time', 'part_time', 'contractor') NOT NULL,
    department_id SMALLINT NOT NULL,
    job_title_id  INT NOT NULL,
    manager_id    INT NULL,
    salary        DECIMAL(10, 2) NOT NULL,
    full_name     VARCHAR(121) GENERATED ALWAYS AS (CONCAT(first_name, ' ', last_name)) STORED,
    UNIQUE KEY uk_employees_email (email),
    CONSTRAINT fk_employees_department FOREIGN KEY (department_id) REFERENCES departments (department_id),
    CONSTRAINT fk_employees_title FOREIGN KEY (job_title_id) REFERENCES job_titles (job_title_id),
    CONSTRAINT fk_employees_manager FOREIGN KEY (manager_id) REFERENCES employees (employee_id),
    CONSTRAINT ck_birth CHECK (birth_date < '2008-01-01'),
    CONSTRAINT ck_salary CHECK (salary > 0),
    CONSTRAINT ck_left CHECK (left_on IS NULL OR left_on > hired_on)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE salary_history (
    employee_id INT NOT NULL,
    valid_from  DATE NOT NULL,
    salary      DECIMAL(10, 2) NOT NULL,
    reason      TEXT NULL,
    PRIMARY KEY (employee_id, valid_from),
    CONSTRAINT fk_history_employee FOREIGN KEY (employee_id) REFERENCES employees (employee_id),
    CONSTRAINT ck_history_salary CHECK (salary > 0)
) ENGINE = InnoDB;

CREATE TABLE timesheets (
    timesheet_id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    employee_id  INT NOT NULL,
    work_date    DATE NOT NULL,
    hours        DECIMAL(4, 2) NOT NULL,
    project_code VARCHAR(12) NOT NULL,
    approved     BOOLEAN NOT NULL DEFAULT FALSE,
    INDEX idx_timesheets_day (work_date, employee_id),
    CONSTRAINT fk_timesheets_employee FOREIGN KEY (employee_id) REFERENCES employees (employee_id),
    CONSTRAINT ck_hours CHECK (hours > 0 AND hours <= 24)
) ENGINE = InnoDB;

-- Later migrations.
ALTER TABLE salary_history CHANGE COLUMN reason change_reason VARCHAR(200) NULL;
