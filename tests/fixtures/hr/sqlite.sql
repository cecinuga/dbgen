-- Human resources in SQLite: two self-referencing hierarchies (departments and managers),
-- a CHECK relating two columns, a history keyed by employee and date, and a generated
-- column.

CREATE TABLE departments (
    department_id        INTEGER PRIMARY KEY,
    name                 TEXT NOT NULL UNIQUE,
    cost_center          TEXT NOT NULL UNIQUE,
    parent_department_id INTEGER REFERENCES departments (department_id)
);

CREATE TABLE job_titles (
    job_title_id INTEGER PRIMARY KEY,
    title        TEXT NOT NULL,
    min_salary   NUMERIC(10, 2) NOT NULL,
    max_salary   NUMERIC(10, 2) NOT NULL,
    CHECK (min_salary > 0 AND max_salary > min_salary)
);

CREATE TABLE employees (
    employee_id   INTEGER PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    first_name    TEXT NOT NULL,
    last_name     TEXT NOT NULL,
    birth_date    DATE NOT NULL CHECK (birth_date < '2008-01-01'),
    hired_on      DATE NOT NULL,
    left_on       DATE,
    kind          TEXT NOT NULL CHECK (kind IN ('full_time', 'part_time', 'contractor')),
    department_id INTEGER NOT NULL REFERENCES departments (department_id),
    job_title_id  INTEGER NOT NULL REFERENCES job_titles (job_title_id),
    manager_id    INTEGER REFERENCES employees (employee_id),
    salary        NUMERIC(10, 2) NOT NULL CHECK (salary > 0),
    full_name     TEXT GENERATED ALWAYS AS (first_name || ' ' || last_name) VIRTUAL,
    CHECK (left_on IS NULL OR left_on > hired_on)
);

CREATE TABLE salary_history (
    employee_id INTEGER NOT NULL REFERENCES employees (employee_id),
    valid_from  DATE NOT NULL,
    salary      NUMERIC(10, 2) NOT NULL CHECK (salary > 0),
    reason      TEXT,
    PRIMARY KEY (employee_id, valid_from)
);

CREATE TABLE timesheets (
    timesheet_id INTEGER PRIMARY KEY AUTOINCREMENT,
    employee_id  INTEGER NOT NULL REFERENCES employees (employee_id),
    work_date    DATE NOT NULL,
    hours        REAL NOT NULL CHECK (hours > 0 AND hours <= 24),
    project_code TEXT NOT NULL,
    approved     BOOLEAN NOT NULL DEFAULT 0
);

CREATE INDEX timesheets_by_day ON timesheets (work_date, employee_id);

-- Later migrations.
ALTER TABLE employees ADD COLUMN phone TEXT;
