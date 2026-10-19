identity :: a -> a
-- ^ @function
identity x = x
-- ^ @function

first, second :: Int -> Int
-- ^ @function
--     ^ @function
first x = x
-- ^ @function
second x = x
-- ^ @function

action :: IO ()
action = print "ok"
-- ^ @function

callback = \x -> x
-- ^ @function
