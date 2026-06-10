-- SQLite mirror of schema.powql, used for the side-by-side baseline pass.
-- Same tables, same columns, same data (see seed.sql). Load with:
--   sqlite3 baseline.db < schema.sql && sqlite3 baseline.db < seed.sql

CREATE TABLE categories (id INTEGER NOT NULL, name TEXT NOT NULL, parent_id INTEGER);
CREATE TABLE users (id INTEGER NOT NULL, name TEXT NOT NULL, email TEXT NOT NULL, city TEXT, age INTEGER, active INTEGER);
CREATE TABLE addresses (id INTEGER NOT NULL, user_id INTEGER NOT NULL, city TEXT, country TEXT, zip TEXT);
CREATE TABLE products (id INTEGER NOT NULL, sku TEXT NOT NULL, name TEXT NOT NULL, category_id INTEGER, price REAL, active INTEGER);
CREATE TABLE inventory (product_id INTEGER NOT NULL, quantity INTEGER NOT NULL, warehouse TEXT);
CREATE TABLE orders (id INTEGER NOT NULL, user_id INTEGER NOT NULL, total REAL, status TEXT, city TEXT);
CREATE TABLE order_items (id INTEGER NOT NULL, order_id INTEGER NOT NULL, product_id INTEGER NOT NULL, quantity INTEGER, unit_price REAL);
CREATE TABLE payments (id INTEGER NOT NULL, order_id INTEGER NOT NULL, amount REAL, method TEXT, status TEXT);
CREATE TABLE reviews (id INTEGER NOT NULL, product_id INTEGER NOT NULL, user_id INTEGER NOT NULL, rating INTEGER, body TEXT);
CREATE TABLE sessions (id INTEGER NOT NULL, user_id INTEGER NOT NULL, token TEXT, active INTEGER);
