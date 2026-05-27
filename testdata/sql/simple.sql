CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    email VARCHAR(255) UNIQUE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    user_id INTEGER REFERENCES users(id),
    total DECIMAL(10, 2),
    status VARCHAR(20) DEFAULT 'pending'
);

CREATE VIEW active_users AS
SELECT u.id, u.name, u.email
FROM users u
WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id);

CREATE FUNCTION calculate_total(order_id INTEGER)
RETURNS DECIMAL AS $$
BEGIN
    RETURN (SELECT SUM(total) FROM orders WHERE id = order_id);
END;
$$ LANGUAGE plpgsql;

CREATE PROCEDURE update_status(
    IN p_order_id INTEGER,
    IN p_status VARCHAR(20)
)
AS $$
BEGIN
    UPDATE orders SET status = p_status WHERE id = p_order_id;
END;
$$ LANGUAGE plpgsql;

CREATE INDEX idx_orders_user_id ON orders(user_id);
