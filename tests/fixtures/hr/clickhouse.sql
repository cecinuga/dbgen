-- Human resources in ClickHouse: the hierarchies are declared in the configuration, the
-- salary history uses VersionedCollapsingMergeTree (sign = 1 keeps rows), and an ALIAS
-- column is computed on read.

CREATE TABLE IF NOT EXISTS departments
(
    department_id        UInt16,
    name                 String,
    cost_center          String,
    parent_department_id Nullable(UInt16)
)
ENGINE = ReplacingMergeTree
ORDER BY department_id;

CREATE TABLE IF NOT EXISTS job_titles
(
    job_title_id UInt32,
    title        String,
    min_salary   Decimal(10, 2),
    max_salary   Decimal(10, 2),
    CONSTRAINT ck_salary_band CHECK min_salary > 0 AND max_salary > min_salary
)
ENGINE = ReplacingMergeTree
ORDER BY job_title_id;

CREATE TABLE IF NOT EXISTS employees
(
    employee_id   UInt32,
    email         String,
    first_name    String,
    last_name     String,
    birth_date    Date32,
    hired_on      Date,
    left_on       Nullable(Date),
    kind          Enum8('full_time' = 1, 'part_time' = 2, 'contractor' = 3),
    department_id UInt16,
    job_title_id  UInt32,
    manager_id    Nullable(UInt32),
    salary        Decimal(10, 2),
    full_name     String ALIAS concat(first_name, ' ', last_name),
    CONSTRAINT ck_birth CHECK birth_date < toDate32('2008-01-01'),
    CONSTRAINT ck_salary CHECK salary > 0,
    CONSTRAINT ck_left CHECK left_on IS NULL OR left_on > hired_on
)
ENGINE = ReplacingMergeTree
ORDER BY employee_id;

CREATE TABLE IF NOT EXISTS salary_history
(
    employee_id UInt32,
    valid_from  Date,
    salary      Decimal(10, 2),
    reason      Nullable(String),
    sign        Int8,
    version     UInt32,
    CONSTRAINT ck_history_salary CHECK salary > 0
)
ENGINE = VersionedCollapsingMergeTree(sign, version)
ORDER BY (employee_id, valid_from);

CREATE TABLE IF NOT EXISTS timesheets
(
    timesheet_id UInt64,
    employee_id  UInt32,
    work_date    Date,
    hours        Decimal(4, 2),
    project_code LowCardinality(String),
    approved     Bool DEFAULT false,
    CONSTRAINT ck_hours CHECK hours > 0 AND hours <= 24
)
ENGINE = MergeTree
PARTITION BY toYear(work_date)
ORDER BY (work_date, employee_id, timesheet_id);
