-- SQLite mirror of seed.powql. Byte-for-byte the same data so the
-- PowDB and SQLite pass rates compare like-for-like. booleans are 0/1.

INSERT INTO categories VALUES (1, 'Electronics', NULL);
INSERT INTO categories VALUES (2, 'Phones', 1);
INSERT INTO categories VALUES (3, 'Laptops', 1);
INSERT INTO categories VALUES (4, 'Books', NULL);

INSERT INTO users VALUES (1, 'Alice', 'alice@ex.com', 'NYC', 30, 1);
INSERT INTO users VALUES (2, 'Bob', 'bob@ex.com', 'NYC', 22, 0);
INSERT INTO users VALUES (3, 'Carol', 'carol@ex.com', 'LA', 45, 1);
INSERT INTO users VALUES (4, 'Dave', 'dave@ex.com', 'LA', 28, 1);
INSERT INTO users VALUES (5, 'Erin', 'erin@ex.com', 'SF', 17, 0);

INSERT INTO addresses VALUES (1, 1, 'NYC', 'US', '10001');
INSERT INTO addresses VALUES (2, 2, 'NYC', 'US', '10002');
INSERT INTO addresses VALUES (3, 3, 'LA', 'US', '90001');
INSERT INTO addresses VALUES (4, 4, 'LA', 'US', '90002');
INSERT INTO addresses VALUES (5, 5, 'SF', 'US', '94101');
INSERT INTO addresses VALUES (6, 1, 'Boston', 'US', '02108');

INSERT INTO products VALUES (1, 'P-PHONE-1', 'Phone X', 2, 699.0, 1);
INSERT INTO products VALUES (2, 'P-PHONE-2', 'Phone Y', 2, 499.0, 1);
INSERT INTO products VALUES (3, 'P-LAP-1', 'Laptop Pro', 3, 1299.0, 1);
INSERT INTO products VALUES (4, 'P-LAP-2', 'Laptop Air', 3, 999.0, 0);
INSERT INTO products VALUES (5, 'P-BOOK-1', 'PowQL Book', 4, 39.0, 1);

INSERT INTO inventory VALUES (1, 50, 'W1');
INSERT INTO inventory VALUES (2, 20, 'W1');
INSERT INTO inventory VALUES (3, 5, 'W2');
INSERT INTO inventory VALUES (4, 0, 'W2');
INSERT INTO inventory VALUES (5, 200, 'W3');

INSERT INTO orders VALUES (1, 1, 699.0, 'paid', 'NYC');
INSERT INTO orders VALUES (2, 1, 39.0, 'paid', 'NYC');
INSERT INTO orders VALUES (3, 2, 499.0, 'pending', 'NYC');
INSERT INTO orders VALUES (4, 3, 1299.0, 'paid', 'LA');
INSERT INTO orders VALUES (5, 4, 999.0, 'paid', 'LA');
INSERT INTO orders VALUES (6, 5, 39.0, 'cancelled', 'SF');

INSERT INTO order_items VALUES (1, 1, 1, 1, 699.0);
INSERT INTO order_items VALUES (2, 2, 5, 1, 39.0);
INSERT INTO order_items VALUES (3, 3, 2, 1, 499.0);
INSERT INTO order_items VALUES (4, 4, 3, 1, 1299.0);
INSERT INTO order_items VALUES (5, 5, 4, 1, 999.0);
INSERT INTO order_items VALUES (6, 6, 5, 1, 39.0);

INSERT INTO payments VALUES (1, 1, 699.0, 'card', 'settled');
INSERT INTO payments VALUES (2, 2, 39.0, 'card', 'settled');
INSERT INTO payments VALUES (3, 3, 499.0, 'paypal', 'pending');
INSERT INTO payments VALUES (4, 4, 1299.0, 'card', 'settled');
INSERT INTO payments VALUES (5, 5, 999.0, 'card', 'settled');

INSERT INTO reviews VALUES (1, 1, 1, 5, 'great');
INSERT INTO reviews VALUES (2, 1, 3, 4, 'good');
INSERT INTO reviews VALUES (3, 3, 4, 3, 'ok');
INSERT INTO reviews VALUES (4, 5, 1, 5, 'love it');

INSERT INTO sessions VALUES (1, 1, 'tok-aaa', 1);
INSERT INTO sessions VALUES (2, 2, 'tok-bbb', 0);
INSERT INTO sessions VALUES (3, 3, 'tok-ccc', 1);
